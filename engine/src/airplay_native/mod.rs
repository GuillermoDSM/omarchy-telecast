//! Experimental AirPlay sender for PIN/HAP receivers without FPSAP.
#![allow(dead_code)]

pub mod capture;
pub mod codec;
pub mod discovery;
pub mod event;
pub mod hap;
pub mod manager;
pub mod pairing;
pub mod rtsp;
pub mod session;
pub mod sources;
pub mod timing;
pub mod transport;
pub mod worker;

#[allow(unused_imports)]
pub use discovery::{Device, discover};
#[allow(unused_imports)]
pub use rtsp::{Client, Response};
