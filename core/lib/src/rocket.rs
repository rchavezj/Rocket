use std::collections::HashMap;
use std::convert::{From, TryInto};
use std::cmp::min;
use std::io;
use std::mem;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;
use std::pin::Pin;

use futures::future::Future;
use futures::stream::StreamExt;
use futures::task::SpawnExt;
use futures_tokio_compat::Compat as TokioCompat;

use yansi::Paint;
use state::Container;

#[cfg(feature = "tls")] use crate::http::tls::TlsAcceptor;

use crate::{logger, handler};
use crate::config::{self, Config, LoggedValue};
use crate::request::{Request, FormItems};
use crate::data::Data;
use crate::response::{Body, Response};
use crate::router::{Router, Route};
use crate::catcher::{self, Catcher};
use crate::outcome::Outcome;
use crate::error::{LaunchError, LaunchErrorKind};
use crate::fairing::{Fairing, Fairings};
use crate::logger::PaintExt;
use crate::ext::AsyncReadExt;

use crate::http::{Method, Status, Header};
use crate::http::hyper::{self, header};
use crate::http::uri::Origin;

/// The main `Rocket` type: used to mount routes and catchers and launch the
/// application.
pub struct Rocket {
    pub(crate) config: Config,
    router: Router,
    default_catchers: HashMap<u16, Catcher>,
    catchers: HashMap<u16, Catcher>,
    pub(crate) state: Container,
    fairings: Fairings,
}

// This function tries to hide all of the Hyper-ness from Rocket. It
// essentially converts Hyper types into Rocket types, then calls the
// `dispatch` function, which knows nothing about Hyper. Because responding
// depends on the `HyperResponse` type, this function does the actual
// response processing.
fn hyper_service_fn(
    rocket: Arc<Rocket>,
    h_addr: std::net::SocketAddr,
    mut spawn: impl futures::task::Spawn,
    hyp_req: hyper::Request<hyper::Body>,
) -> impl Future<Output = Result<hyper::Response<hyper::Body>, io::Error>> {
    // This future must return a hyper::Response, but that's not easy
    // because the response body might borrow from the request. Instead,
    // we do the body writing in another future that will send us
    // the response metadata (and a body channel) beforehand.
    let (tx, rx) = futures::channel::oneshot::channel();

    spawn.spawn(async move {
        // Get all of the information from Hyper.
        let (h_parts, h_body) = hyp_req.into_parts();

        // Convert the Hyper request into a Rocket request.
        let req_res = Request::from_hyp(&rocket, h_parts.method, h_parts.headers, h_parts.uri, h_addr);
        let mut req = match req_res {
            Ok(req) => req,
            Err(e) => {
                error!("Bad incoming request: {}", e);
                // TODO: We don't have a request to pass in, so we just
                // fabricate one. This is weird. We should let the user know
                // that we failed to parse a request (by invoking some special
                // handler) instead of doing this.
                let dummy = Request::new(&rocket, Method::Get, Origin::dummy());
                let r = rocket.handle_error(Status::BadRequest, &dummy).await;
                return rocket.issue_response(r, tx).await;
            }
        };

        // Retrieve the data from the hyper body.
        let data = Data::from_hyp(h_body).await;

        // Dispatch the request to get a response, then write that response out.
        let r = rocket.dispatch(&mut req, data).await;
        rocket.issue_response(r, tx).await;
    }).expect("failed to spawn handler");

    async move {
        Ok(rx.await.expect("TODO.async: sender was dropped, error instead"))
    }
}

impl Rocket {
    // TODO.async: Reconsider io::Result
    #[inline]
    fn issue_response<'r>(
        &self,
        response: Response<'r>,
        tx: futures::channel::oneshot::Sender<hyper::Response<hyper::Body>>,
    ) -> impl Future<Output = ()> + 'r {
        let result = self.write_response(response, tx);
        async move {
            match result.await {
                Ok(()) => {
                    info_!("{}", Paint::green("Response succeeded."));
                }
                Err(e) => {
                    error_!("Failed to write response: {:?}.", e);
                }
            }
        }
    }

    #[inline]
    fn write_response<'r>(
        &self,
        mut response: Response<'r>,
        tx: futures::channel::oneshot::Sender<hyper::Response<hyper::Body>>,
    ) -> impl Future<Output = io::Result<()>> + 'r {
        async move {
            let mut hyp_res = hyper::Response::builder();
            hyp_res.status(response.status().code);

            for header in response.headers().iter() {
                let name = header.name.as_str();
                let value = header.value.as_bytes();
                hyp_res.header(name, value);
            }

            let send_response = move |mut hyp_res: hyper::ResponseBuilder, body| -> io::Result<()> {
                let response = hyp_res.body(body).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                tx.send(response).expect("channel receiver should not be dropped");
                Ok(())
            };

            match response.body() {
                None => {
                    hyp_res.header(header::CONTENT_LENGTH, "0");
                    send_response(hyp_res, hyper::Body::empty())?;
                }
                Some(Body::Sized(body, size)) => {
                    hyp_res.header(header::CONTENT_LENGTH, size.to_string());
                    let (mut sender, hyp_body) = hyper::Body::channel();
                    send_response(hyp_res, hyp_body)?;

                    let mut stream = body.into_chunk_stream(4096);
                    while let Some(next) = stream.next().await {
                        futures::future::poll_fn(|cx| sender.poll_ready(cx)).await.expect("TODO.async client gone?");
                        sender.send_data(next?).expect("send chunk");
                    }
                }
                Some(Body::Chunked(body, chunk_size)) => {
                    // TODO.async: This is identical to Body::Sized except for the chunk size

                    let (mut sender, hyp_body) = hyper::Body::channel();
                    send_response(hyp_res, hyp_body)?;

                    let mut stream = body.into_chunk_stream(chunk_size.try_into().expect("u64 -> usize overflow"));
                    while let Some(next) = stream.next().await {
                        futures::future::poll_fn(|cx| sender.poll_ready(cx)).await.expect("TODO.async client gone?");
                        sender.send_data(next?).expect("send chunk");
                    }
                }
            };

            Ok(())
        }
    }
}

impl Rocket {
    /// Preprocess the request for Rocket things. Currently, this means:
    ///
    ///   * Rewriting the method in the request if _method form field exists.
    ///
    /// Keep this in-sync with derive_form when preprocessing form fields.
    fn preprocess_request(&self, req: &mut Request<'_>, data: &Data) {
        // Check if this is a form and if the form contains the special _method
        // field which we use to reinterpret the request's method.
        let data_len = data.peek().len();
        let (min_len, max_len) = ("_method=get".len(), "_method=delete".len());
        let is_form = req.content_type().map_or(false, |ct| ct.is_form());

        if is_form && req.method() == Method::Post && data_len >= min_len {
            if let Ok(form) = std::str::from_utf8(&data.peek()[..min(data_len, max_len)]) {
                let method: Option<Result<Method, _>> = FormItems::from(form)
                    .filter(|item| item.key.as_str() == "_method")
                    .map(|item| item.value.parse())
                    .next();

                if let Some(Ok(method)) = method {
                    req.set_method(method);
                }
            }
        }
    }

    #[inline]
    pub(crate) fn dispatch<'s, 'r: 's>(
        &'s self,
        request: &'r mut Request<'s>,
        data: Data
    ) -> impl Future<Output = Response<'r>> + 's {
        async move {
            info!("{}:", request);

            // Do a bit of preprocessing before routing.
            self.preprocess_request(request, &data);

            // Run the request fairings.
            self.fairings.handle_request(request, &data);

            // Remember if the request is a `HEAD` request for later body stripping.
            let was_head_request = request.method() == Method::Head;

            // Route the request and run the user's handlers.
            let mut response = self.route_and_process(request, data).await;

            // Add a default 'Server' header if it isn't already there.
            // TODO: If removing Hyper, write out `Date` header too.
            if !response.headers().contains("Server") {
                response.set_header(Header::new("Server", "Rocket"));
            }

            // Run the response fairings.
            self.fairings.handle_response(request, &mut response).await;

            // Strip the body if this is a `HEAD` request.
            if was_head_request {
                response.strip_body();
            }

            response
        }
    }

    /// Route the request and process the outcome to eventually get a response.
    fn route_and_process<'s, 'r: 's>(
        &'s self,
        request: &'r Request<'s>,
        data: Data
    ) -> impl Future<Output = Response<'r>> + Send + 's {
        async move {
            let mut response = match self.route(request, data).await {
                Outcome::Success(response) => response,
                Outcome::Forward(data) => {
                    // There was no matching route. Autohandle `HEAD` requests.
                    if request.method() == Method::Head {
                        info_!("Autohandling {} request.", Paint::default("HEAD").bold());

                        // Dispatch the request again with Method `GET`.
                        request._set_method(Method::Get);

                        // Return early so we don't set cookies twice.
                        let try_next: Pin<Box<dyn Future<Output = _> + Send>> = Box::pin(self.route_and_process(request, data));
                        return try_next.await;
                    } else {
                        // No match was found and it can't be autohandled. 404.
                        self.handle_error(Status::NotFound, request).await
                    }
                }
                Outcome::Failure(status) => self.handle_error(status, request).await,
            };

            // Set the cookies. Note that error responses will only include
            // cookies set by the error handler. See `handle_error` for more.
            for cookie in request.cookies().delta() {
                response.adjoin_header(cookie);
            }

            response
        }
    }

    /// Tries to find a `Responder` for a given `request`. It does this by
    /// routing the request and calling the handler for each matching route
    /// until one of the handlers returns success or failure, or there are no
    /// additional routes to try (forward). The corresponding outcome for each
    /// condition is returned.
    //
    // TODO: We _should_ be able to take an `&mut` here and mutate the request
    // at any pointer _before_ we pass it to a handler as long as we drop the
    // outcome. That should be safe. Since no mutable borrow can be held
    // (ensuring `handler` takes an immutable borrow), any caller to `route`
    // should be able to supply an `&mut` and retain an `&` after the call.
    #[inline]
    pub(crate) fn route<'s, 'r: 's>(
        &'s self,
        request: &'r Request<'s>,
        mut data: Data,
    ) -> impl Future<Output = handler::Outcome<'r>> + 's {
        async move {
            // Go through the list of matching routes until we fail or succeed.
            let matches = self.router.route(request);
            for route in matches {
                // Retrieve and set the requests parameters.
                info_!("Matched: {}", route);
                request.set_route(route);

                // Dispatch the request to the handler.
                let outcome = route.handler.handle(request, data).await;

                // Check if the request processing completed (Some) or if the request needs
                // to be forwarded. If it does, continue the loop (None) to try again.
                info_!("{} {}", Paint::default("Outcome:").bold(), outcome);
                match outcome {
                    o@Outcome::Success(_) | o@Outcome::Failure(_) => return o,
                    Outcome::Forward(unused_data) => data = unused_data,
                }
            }

            error_!("No matching routes for {}.", request);
            Outcome::Forward(data)
        }
    }

    // Finds the error catcher for the status `status` and executes it for the
    // given request `req`. If a user has registered a catcher for `status`, the
    // catcher is called. If the catcher fails to return a good response, the
    // 500 catcher is executed. If there is no registered catcher for `status`,
    // the default catcher is used.
    pub(crate) fn handle_error<'s, 'r: 's>(
        &'s self,
        status: Status,
        req: &'r Request<'s>
    ) -> impl Future<Output = Response<'r>> + 's {
        async move {
            warn_!("Responding with {} catcher.", Paint::red(&status));

            // For now, we reset the delta state to prevent any modifications
            // from earlier, unsuccessful paths from being reflected in error
            // response. We may wish to relax this in the future.
            req.cookies().reset_delta();

            // Try to get the active catcher but fallback to user's 500 catcher.
            let catcher = self.catchers.get(&status.code).unwrap_or_else(|| {
                error_!("No catcher found for {}. Using 500 catcher.", status);
                self.catchers.get(&500).expect("500 catcher.")
            });

            // Dispatch to the user's catcher. If it fails, use the default 500.
            match catcher.handle(req).await {
                Ok(r) => return r,
                Err(err_status) => {
                    error_!("Catcher failed with status: {}!", err_status);
                    warn_!("Using default 500 error catcher.");
                    let default = self.default_catchers.get(&500).expect("Default 500");
                    default.handle(req).await.expect("Default 500 response.")
                }
            }
        }
    }
}

impl Rocket {
    /// Create a new `Rocket` application using the configuration information in
    /// `Rocket.toml`. If the file does not exist or if there is an I/O error
    /// reading the file, the defaults are used. See the [`config`]
    /// documentation for more information on defaults.
    ///
    /// This method is typically called through the
    /// [`rocket::ignite()`](crate::ignite) alias.
    ///
    /// # Panics
    ///
    /// If there is an error parsing the `Rocket.toml` file, this functions
    /// prints a nice error message and then exits the process.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # {
    /// rocket::ignite()
    /// # };
    /// ```
    #[inline]
    pub fn ignite() -> Rocket {
        // Note: init() will exit the process under config errors.
        Rocket::configured(config::init())
    }

    /// Creates a new `Rocket` application using the supplied custom
    /// configuration. The `Rocket.toml` file, if present, is ignored. Any
    /// environment variables setting config parameters are ignored.
    ///
    /// This method is typically called through the `rocket::custom` alias.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rocket::config::{Config, Environment};
    /// # use rocket::config::ConfigError;
    ///
    /// # #[allow(dead_code)]
    /// # fn try_config() -> Result<(), ConfigError> {
    /// let config = Config::build(Environment::Staging)
    ///     .address("1.2.3.4")
    ///     .port(9234)
    ///     .finalize()?;
    ///
    /// # #[allow(unused_variables)]
    /// let app = rocket::custom(config);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn custom(config: Config) -> Rocket {
        Rocket::configured(config)
    }

    #[inline]
    fn configured(config: Config) -> Rocket {
        if logger::try_init(config.log_level, false) {
            // Temporary weaken log level for launch info.
            logger::push_max_level(logger::LoggingLevel::Normal);
        }

        launch_info!("{}Configured for {}.", Paint::emoji("🔧 "), config.environment);
        launch_info_!("address: {}", Paint::default(&config.address).bold());
        launch_info_!("port: {}", Paint::default(&config.port).bold());
        launch_info_!("log: {}", Paint::default(config.log_level).bold());
        launch_info_!("workers: {}", Paint::default(config.workers).bold());
        launch_info_!("secret key: {}", Paint::default(&config.secret_key).bold());
        launch_info_!("limits: {}", Paint::default(&config.limits).bold());

        match config.keep_alive {
            Some(v) => launch_info_!("keep-alive: {}", Paint::default(format!("{}s", v)).bold()),
            None => launch_info_!("keep-alive: {}", Paint::default("disabled").bold()),
        }

        let tls_configured = config.tls.is_some();
        if tls_configured && cfg!(feature = "tls") {
            launch_info_!("tls: {}", Paint::default("enabled").bold());
        } else if tls_configured {
            error_!("tls: {}", Paint::default("disabled").bold());
            error_!("tls is configured, but the tls feature is disabled");
        } else {
            launch_info_!("tls: {}", Paint::default("disabled").bold());
        }

        if config.secret_key.is_generated() && config.environment.is_prod() {
            warn!("environment is 'production', but no `secret_key` is configured");
        }

        for (name, value) in config.extras() {
            launch_info_!("{} {}: {}",
                          Paint::yellow("[extra]"), name,
                          Paint::default(LoggedValue(value)).bold());
        }

        Rocket {
            config,
            router: Router::new(),
            default_catchers: catcher::defaults::get(),
            catchers: catcher::defaults::get(),
            state: Container::new(),
            fairings: Fairings::new(),
        }
    }

    /// Mounts all of the routes in the supplied vector at the given `base`
    /// path. Mounting a route with path `path` at path `base` makes the route
    /// available at `base/path`.
    ///
    /// # Panics
    ///
    /// Panics if the `base` mount point is not a valid static path: a valid
    /// origin URI without dynamic parameters.
    ///
    /// Panics if any route's URI is not a valid origin URI. This kind of panic
    /// is guaranteed not to occur if the routes were generated using Rocket's
    /// code generation.
    ///
    /// # Examples
    ///
    /// Use the `routes!` macro to mount routes created using the code
    /// generation facilities. Requests to the `/hello/world` URI will be
    /// dispatched to the `hi` route.
    ///
    /// ```rust
    /// # #![feature(proc_macro_hygiene, async_await)]
    /// # #[macro_use] extern crate rocket;
    /// #
    /// #[get("/world")]
    /// fn hi() -> &'static str {
    ///     "Hello!"
    /// }
    ///
    /// fn main() {
    /// # if false { // We don't actually want to launch the server in an example.
    ///     rocket::ignite().mount("/hello", routes![hi])
    /// #       .launch();
    /// # }
    /// }
    /// ```
    ///
    /// Manually create a route named `hi` at path `"/world"` mounted at base
    /// `"/hello"`. Requests to the `/hello/world` URI will be dispatched to the
    /// `hi` route.
    ///
    /// ```rust
    /// use rocket::{Request, Route, Data};
    /// use rocket::handler::{HandlerFuture, Outcome};
    /// use rocket::http::Method::*;
    ///
    /// fn hi<'r>(req: &'r Request, _: Data) -> HandlerFuture<'r> {
    ///     Outcome::from(req, "Hello!")
    /// }
    ///
    /// # if false { // We don't actually want to launch the server in an example.
    /// rocket::ignite().mount("/hello", vec![Route::new(Get, "/world", hi)])
    /// #     .launch();
    /// # }
    /// ```
    #[inline]
    pub fn mount<R: Into<Vec<Route>>>(mut self, base: &str, routes: R) -> Self {
        info!("{}{} {}{}",
              Paint::emoji("🛰  "),
              Paint::magenta("Mounting"),
              Paint::blue(base),
              Paint::magenta(":"));

        let base_uri = Origin::parse(base)
            .unwrap_or_else(|e| {
                error_!("Invalid origin URI '{}' used as mount point.", base);
                panic!("Error: {}", e);
            });

        if base_uri.query().is_some() {
            error_!("Mount point '{}' contains query string.", base);
            panic!("Invalid mount point.");
        }

        for mut route in routes.into() {
            let path = route.uri.clone();
            if let Err(e) = route.set_uri(base_uri.clone(), path) {
                error_!("{}", e);
                panic!("Invalid route URI.");
            }

            info_!("{}", route);
            self.router.add(route);
        }

        self
    }

    /// Registers all of the catchers in the supplied vector.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # #![feature(proc_macro_hygiene, async_await)]
    /// # #[macro_use] extern crate rocket;
    /// use rocket::Request;
    ///
    /// #[catch(500)]
    /// fn internal_error() -> &'static str {
    ///     "Whoops! Looks like we messed up."
    /// }
    ///
    /// #[catch(400)]
    /// fn not_found(req: &Request) -> String {
    ///     format!("I couldn't find '{}'. Try something else?", req.uri())
    /// }
    ///
    /// fn main() {
    /// # if false { // We don't actually want to launch the server in an example.
    ///     rocket::ignite()
    ///         .register(catchers![internal_error, not_found])
    /// #       .launch();
    /// # }
    /// }
    /// ```
    #[inline]
    pub fn register(mut self, catchers: Vec<Catcher>) -> Self {
        info!("{}{}", Paint::emoji("👾 "), Paint::magenta("Catchers:"));

        for c in catchers {
            if self.catchers.get(&c.code).map_or(false, |e| !e.is_default) {
                info_!("{} {}", c, Paint::yellow("(warning: duplicate catcher!)"));
            } else {
                info_!("{}", c);
            }

            self.catchers.insert(c.code, c);
        }

        self
    }

    /// Add `state` to the state managed by this instance of Rocket.
    ///
    /// This method can be called any number of times as long as each call
    /// refers to a different `T`.
    ///
    /// Managed state can be retrieved by any request handler via the
    /// [`State`](crate::State) request guard. In particular, if a value of type `T`
    /// is managed by Rocket, adding `State<T>` to the list of arguments in a
    /// request handler instructs Rocket to retrieve the managed value.
    ///
    /// # Panics
    ///
    /// Panics if state of type `T` is already being managed.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(proc_macro_hygiene, async_await)]
    /// # #[macro_use] extern crate rocket;
    /// use rocket::State;
    ///
    /// struct MyValue(usize);
    ///
    /// #[get("/")]
    /// fn index(state: State<MyValue>) -> String {
    ///     format!("The stateful value is: {}", state.0)
    /// }
    ///
    /// fn main() {
    /// # if false { // We don't actually want to launch the server in an example.
    ///     rocket::ignite()
    ///         .mount("/", routes![index])
    ///         .manage(MyValue(10))
    ///         .launch();
    /// # }
    /// }
    /// ```
    #[inline]
    pub fn manage<T: Send + Sync + 'static>(self, state: T) -> Self {
        if !self.state.set::<T>(state) {
            error!("State for this type is already being managed!");
            panic!("Aborting due to duplicately managed state.");
        }

        self
    }

    /// Attaches a fairing to this instance of Rocket. If the fairing is an
    /// _attach_ fairing, it is run immediately. All other kinds of fairings
    /// will be executed at their appropriate time.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(proc_macro_hygiene, async_await)]
    /// # #[macro_use] extern crate rocket;
    /// use rocket::Rocket;
    /// use rocket::fairing::AdHoc;
    ///
    /// fn main() {
    /// # if false { // We don't actually want to launch the server in an example.
    ///     rocket::ignite()
    ///         .attach(AdHoc::on_launch("Launch Message", |_| {
    ///             println!("Rocket is launching!");
    ///         }))
    ///         .launch();
    /// # }
    /// }
    /// ```
    #[inline]
    pub fn attach<F: Fairing>(mut self, fairing: F) -> Self {
        // Attach (and run attach) fairings, which requires us to move `self`.
        let mut fairings = mem::replace(&mut self.fairings, Fairings::new());
        self = fairings.attach(Box::new(fairing), self);

        // Make sure we keep all fairings around: the old and newly added ones!
        fairings.append(self.fairings);
        self.fairings = fairings;
        self
    }

    pub(crate) fn prelaunch_check(mut self) -> Result<Rocket, LaunchError> {
        self.router = match self.router.collisions() {
            Ok(router) => router,
            Err(e) => return Err(LaunchError::new(LaunchErrorKind::Collision(e)))
        };

        if let Some(failures) = self.fairings.failures() {
            return Err(LaunchError::new(LaunchErrorKind::FailedFairings(failures.to_vec())))
        }

        Ok(self)
    }

    /// Starts the application server and begins listening for and dispatching
    /// requests to mounted routes and catchers. Unless there is an error, this
    /// function does not return and blocks until program termination.
    ///
    /// # Error
    ///
    /// If there is a problem starting the application, a [`LaunchError`] is
    /// returned. Note that a value of type `LaunchError` panics if dropped
    /// without first being inspected. See the [`LaunchError`] documentation for
    /// more information.
    ///
    /// # Example
    ///
    /// ```rust
    /// # if false {
    /// rocket::ignite().launch();
    /// # }
    /// ```
    pub fn launch(mut self) -> LaunchError {
        #[cfg(feature = "tls")] use crate::http::tls;

        self = match self.prelaunch_check() {
            Ok(rocket) => rocket,
            Err(launch_error) => return launch_error
        };

        self.fairings.pretty_print_counts();

        // TODO.async What meaning should config.workers have now?
        // Initialize the tokio runtime
        let runtime = tokio::runtime::Builder::new()
            .core_threads(self.config.workers as usize)
            .build()
            .expect("Cannot build runtime!");

        let full_addr = format!("{}:{}", self.config.address, self.config.port);
        let addrs = match full_addr.to_socket_addrs() {
            Ok(a) => a.collect::<Vec<_>>(),
            // TODO.async: Reconsider this error type
            Err(e) => return From::from(io::Error::new(io::ErrorKind::Other, e)),
        };

        // TODO.async: support for TLS, unix sockets.
        // Likely will be implemented with a custom "Incoming" type.

        let mut incoming = match hyper::AddrIncoming::bind(&addrs[0]) {
            Ok(incoming) => incoming,
            Err(e) => return LaunchError::new(LaunchErrorKind::Bind(e)),
        };

        // Determine the address and port we actually binded to.
        self.config.port = incoming.local_addr().port();

        let proto = "http://";

        // Set the keep-alive.
        let timeout = self.config.keep_alive.map(|s| Duration::from_secs(s as u64));
        incoming.set_keepalive(timeout);

        // Freeze managed state for synchronization-free accesses later.
        self.state.freeze();

        // Run the launch fairings.
        self.fairings.handle_launch(&self);

        launch_info!("{}{} {}{}",
                     Paint::emoji("🚀 "),
                     Paint::default("Rocket has launched from").bold(),
                     Paint::default(proto).bold().underline(),
                     Paint::default(&full_addr).bold().underline());

        // Restore the log level back to what it originally was.
        logger::pop_max_level();

        let rocket = Arc::new(self);
        let spawn = Box::new(TokioCompat::new(runtime.executor()));
        let service = hyper::make_service_fn(move |socket: &hyper::AddrStream| {
            let rocket = rocket.clone();
            let remote_addr = socket.remote_addr();
            let spawn = spawn.clone();
            async move {
                Ok::<_, std::convert::Infallible>(hyper::service_fn(move |req| {
                    hyper_service_fn(rocket.clone(), remote_addr, spawn.clone(), req)
                }))
            }
        });

        // NB: executor must be passed manually here, see hyperium/hyper#1537
        let server = hyper::Server::builder(incoming)
            .executor(runtime.executor())
            .serve(service);

        // TODO.async: Use with_graceful_shutdown, and let launch() return a Result<(), Error>
        runtime.block_on(server).expect("TODO.async handle error");

        unreachable!("the call to `block_on` should block on success")
    }

    /// Returns an iterator over all of the routes mounted on this instance of
    /// Rocket.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(proc_macro_hygiene, async_await)]
    /// # #[macro_use] extern crate rocket;
    /// use rocket::Rocket;
    /// use rocket::fairing::AdHoc;
    ///
    /// #[get("/hello")]
    /// fn hello() -> &'static str {
    ///     "Hello, world!"
    /// }
    ///
    /// fn main() {
    ///     let rocket = rocket::ignite()
    ///         .mount("/", routes![hello])
    ///         .mount("/hi", routes![hello]);
    ///
    ///     for route in rocket.routes() {
    ///         match route.base() {
    ///             "/" => assert_eq!(route.uri.path(), "/hello"),
    ///             "/hi" => assert_eq!(route.uri.path(), "/hi/hello"),
    ///             _ => unreachable!("only /hello, /hi/hello are expected")
    ///         }
    ///     }
    ///
    ///     assert_eq!(rocket.routes().count(), 2);
    /// }
    /// ```
    #[inline(always)]
    pub fn routes<'a>(&'a self) -> impl Iterator<Item = &'a Route> + 'a {
        self.router.routes()
    }

    /// Returns `Some` of the managed state value for the type `T` if it is
    /// being managed by `self`. Otherwise, returns `None`.
    ///
    /// # Example
    ///
    /// ```rust
    /// #[derive(PartialEq, Debug)]
    /// struct MyState(&'static str);
    ///
    /// let rocket = rocket::ignite().manage(MyState("hello!"));
    /// assert_eq!(rocket.state::<MyState>(), Some(&MyState("hello!")));
    ///
    /// let client = rocket::local::Client::new(rocket).expect("valid rocket");
    /// assert_eq!(client.rocket().state::<MyState>(), Some(&MyState("hello!")));
    /// ```
    #[inline(always)]
    pub fn state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.state.try_get()
    }

    /// Returns the active configuration.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(proc_macro_hygiene, async_await)]
    /// # #[macro_use] extern crate rocket;
    /// use rocket::Rocket;
    /// use rocket::fairing::AdHoc;
    ///
    /// fn main() {
    /// # if false { // We don't actually want to launch the server in an example.
    ///     rocket::ignite()
    ///         .attach(AdHoc::on_launch("Config Printer", |rocket| {
    ///             println!("Rocket launch config: {:?}", rocket.config());
    ///         }))
    ///         .launch();
    /// # }
    /// }
    /// ```
    #[inline(always)]
    pub fn config(&self) -> &Config {
        &self.config
    }
}
