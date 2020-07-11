#![feature(proc_macro_hygiene, async_await)]

#[macro_use] extern crate rocket;

mod common;

#[test]
fn test_staging_config() {
    common::test_config(rocket::config::Environment::Staging);
}
