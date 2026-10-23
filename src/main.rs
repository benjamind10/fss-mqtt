//! fss-mqtt — interactive terminal MQTT explorer, and its web version.

// The web version leaves out what's for the terminal and the desktop.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

mod app;
mod arrow;
mod avro;
mod base64;
mod brand;
mod broker;
mod capture;
mod changeset;
mod chart;
mod clipboard;
mod composer;
mod compressed;
mod config;
mod crc32;
#[cfg(any(target_arch = "wasm32", test))]
mod demo;
mod fmt;
mod glyph;
mod gzip;
#[cfg(not(target_arch = "wasm32"))]
mod http;
mod input;
mod json;
#[cfg(not(target_arch = "wasm32"))]
mod keys;
mod mqtt;
mod omarchy;
mod parquet;
#[cfg(not(target_arch = "wasm32"))]
mod pemkey;
mod pqzip;
mod pretty;
mod protobuf;
mod proxy;
mod record;
mod session;
mod sparkplug;
mod sqlite;
mod store;
mod theme;
mod ui;
mod update;
mod upgrade;
mod values;
mod view;
mod viewer;
#[cfg(target_arch = "wasm32")]
mod web;
mod ws;

#[cfg(not(target_arch = "wasm32"))]
mod desktop;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    desktop::main();
}

#[cfg(target_arch = "wasm32")]
fn main() {
    web::main();
}
