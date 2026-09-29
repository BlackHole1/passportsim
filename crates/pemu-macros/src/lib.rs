//! Proc macros of PassportSim: `#[command]` registration. Each entry point is a shim that delegates
//! to its owner's file.

use proc_macro::TokenStream;

mod command;

/// `#[command]`: registers a command in the registry. Implemented in `command.rs`.
#[proc_macro_attribute]
pub fn command(attr: TokenStream, item: TokenStream) -> TokenStream {
    command::expand(attr.into(), item.into()).into()
}
