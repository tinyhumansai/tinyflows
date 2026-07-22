//! Browser automation protocol and host-side tool routing.
//!
//! This module is intentionally outside the workflow engine. Browser actions
//! remain ordinary `tool_call` nodes with the exact slug `"browser"`; hosts
//! opt into the Chrome companion by installing a [`RoutingToolInvoker`].

mod protocol;
mod routing;

pub use protocol::{
    BROWSER_PROTOCOL_VERSION, BrowserAction, BrowserError, BrowserErrorCode, BrowserEvent,
    BrowserRequest, BrowserResponse, BrowserResult,
};
pub use routing::{BrowserRelay, ChromeToolInvoker, RoutingToolInvoker};
