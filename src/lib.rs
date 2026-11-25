//! Knihovní jádro LrCatFix - vše kromě CLI/GUI vstupního bodu.
//!
//! `main.rs` je tenký obal, který jen parsuje argumenty a volá funkce odsud.
//! Díky tomu jde jádro otestovat integračními testy v `tests/` a časem ho
//! bude sdílet i `app/` (egui vrstva, CLAUDE.md §5: „app/ nesmí obsahovat SQL
//! ani přímé I/O, volá jen funkce z catalog/, fsops/, index/").

pub mod catalog;
pub mod cluster;
pub mod config;
pub mod fsops;
pub mod index;
pub mod journal;
pub mod matching;
pub mod session;
