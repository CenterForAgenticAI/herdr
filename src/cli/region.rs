#[path = "region_bindings.rs"]
pub(crate) mod bindings;

pub(crate) use bindings::command;

pub(super) fn run_region_command(args: &[String]) -> std::io::Result<i32> {
    super::run_json_request(bindings::request(args))
}
