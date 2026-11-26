//! pacz library. Pure logic lives in the modules with unit tests; process and terminal
//! I/O is confined to `driver` (aria2), `app` (pacman/paru, prompts) and `sys` (libc).
pub mod app;
pub mod aria2;
pub mod aur;
pub mod conf;
pub mod config;
pub mod driver;
pub mod fmt;
pub mod pacman;
pub mod plan;
pub mod render;
pub mod rows;
pub mod sched;
pub mod select;
pub mod sys;
pub mod urls;
