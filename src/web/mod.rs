pub mod action_dispatch;
pub mod adapters;
pub mod bindings;
pub mod budget;
pub mod contracts;
pub mod dispatch;
pub mod handler_call;
pub mod http_problem;
pub mod http_request;
pub mod ir;
pub mod modules;
pub mod openapi;
pub mod package_contracts;
pub mod reactivity;
pub mod request_bindings;
pub mod route_check;
pub mod runtime_bindings;
// This non-executing admission seam is staged for an authenticated HTTP
// integration; do not expose it as production authority until that exists.
#[allow(dead_code)]
pub(crate) mod session_action_admission;
pub mod validation;
