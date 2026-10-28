//! CLI inputs and the `herdr region` command group.

use std::collections::HashMap;

use clap::Command;

use crate::api::schema::regions::{
    RegionOpenProjection, RegionResizeProjection, RegionTargetProjection,
};
use crate::api::schema::{EmptyParams, Method, Request};

pub(crate) use crate::cli::projection::boolean;
use crate::cli::projection::{self, cli_projection, Input, Leaf, ParseError};

pub(crate) fn positional_text(
    name: &'static str,
    index: usize,
    value_name: &'static str,
) -> Input<String> {
    Input::positional(name, index, |slot, value| {
        *slot = value.to_owned();
        Ok(())
    })
    .value_name(value_name)
}

pub(crate) fn region_id() -> Input<String> {
    positional_text("region_id", 0, "REGION_ID")
}

fn parse_size(value: &str) -> Result<u32, ParseError> {
    value
        .parse()
        .map_err(|_| ParseError::Usage(format!("invalid region size: {value}")))
}

pub(crate) fn optional_size() -> Input<Option<u32>> {
    Input::value("size", "CELLS", |slot, value| {
        *slot = Some(parse_size(value)?);
        Ok(())
    })
    .help("Width in cells; clamped to the region's bounds")
}

pub(crate) fn required_size() -> Input<u32> {
    Input::positional("size", 1, |slot, value| {
        *slot = parse_size(value)?;
        Ok(())
    })
    .value_name("CELLS")
}

pub(crate) fn environment() -> Input<HashMap<String, String>> {
    Input::<HashMap<String, String>>::value("env", "KEY=VALUE", |slot, value| {
        let (key, value) = crate::cli::parse_env_assignment(value)?;
        slot.insert(key, value);
        Ok(())
    })
    .help("Set an environment variable for the region process")
}

cli_projection! {
    pub(crate) RegionListProjection for EmptyParams {}
}

const LEAVES: [Leaf; 7] = [
    Leaf {
        name: "open",
        about: "Open a plugin region",
        id: "cli:region:open",
        arguments: RegionOpenProjection::arguments,
        method: |matches| RegionOpenProjection::from_matches(matches).map(Method::RegionOpen),
    },
    Leaf {
        name: "close",
        about: "Close a region and stop its process",
        id: "cli:region:close",
        arguments: RegionTargetProjection::arguments,
        method: |matches| RegionTargetProjection::from_matches(matches).map(Method::RegionClose),
    },
    Leaf {
        name: "focus",
        about: "Focus a visible region",
        id: "cli:region:focus",
        arguments: RegionTargetProjection::arguments,
        method: |matches| RegionTargetProjection::from_matches(matches).map(Method::RegionFocus),
    },
    Leaf {
        name: "resize",
        about: "Set a region's width in cells",
        id: "cli:region:resize",
        arguments: RegionResizeProjection::arguments,
        method: |matches| RegionResizeProjection::from_matches(matches).map(Method::RegionResize),
    },
    Leaf {
        name: "show",
        about: "Show a hidden region",
        id: "cli:region:show",
        arguments: RegionTargetProjection::arguments,
        method: |matches| RegionTargetProjection::from_matches(matches).map(Method::RegionShow),
    },
    Leaf {
        name: "hide",
        about: "Hide a region without stopping it",
        id: "cli:region:hide",
        arguments: RegionTargetProjection::arguments,
        method: |matches| RegionTargetProjection::from_matches(matches).map(Method::RegionHide),
    },
    Leaf {
        name: "list",
        about: "List open regions",
        id: "cli:region:list",
        arguments: RegionListProjection::arguments,
        method: |matches| RegionListProjection::from_matches(matches).map(Method::RegionList),
    },
];

pub(crate) fn command() -> Command {
    Command::new("region")
        .about("Manage plugin regions")
        .subcommand_required(true)
        .subcommands(LEAVES.iter().map(Leaf::command))
}

pub(crate) fn request(args: &[String]) -> Result<Request, ParseError> {
    projection::group_request(command(), &LEAVES, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> serde_json::Value {
        let request = request(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
            .expect("region command parses");
        serde_json::to_value(request).expect("request serializes")
    }

    #[test]
    fn region_open_maps_flags_to_params() {
        assert_eq!(
            parse(&[
                "open",
                "demo.plugin",
                "panel",
                "--size",
                "40",
                "--focus",
                "--env",
                "A=1"
            ]),
            serde_json::json!({
                "id": "cli:region:open",
                "method": "region.open",
                "params": {
                    "plugin_id": "demo.plugin",
                    "entrypoint": "panel",
                    "size": 40,
                    "focus": true,
                    "env": {"A": "1"}
                }
            })
        );
        assert_eq!(
            parse(&["open", "demo.plugin", "panel"]),
            serde_json::json!({
                "id": "cli:region:open",
                "method": "region.open",
                "params": {"plugin_id": "demo.plugin", "entrypoint": "panel"}
            })
        );
    }

    #[test]
    fn region_target_commands_name_their_methods() {
        for (verb, method) in [
            ("close", "region.close"),
            ("focus", "region.focus"),
            ("show", "region.show"),
            ("hide", "region.hide"),
        ] {
            assert_eq!(
                parse(&[verb, "region_1"]),
                serde_json::json!({
                    "id": format!("cli:region:{verb}"),
                    "method": method,
                    "params": {"region_id": "region_1"}
                })
            );
        }
        assert_eq!(
            parse(&["resize", "region_1", "12"]),
            serde_json::json!({
                "id": "cli:region:resize",
                "method": "region.resize",
                "params": {"region_id": "region_1", "size": 12}
            })
        );
        assert_eq!(
            parse(&["list"]),
            serde_json::json!({"id": "cli:region:list", "method": "region.list", "params": {}})
        );
    }

    #[test]
    fn region_size_rejects_non_numbers() {
        let args = ["resize", "region_1", "wide"].map(str::to_owned);
        assert!(request(&args).is_err());
        let args = ["open", "p", "e", "--size", "-3"].map(str::to_owned);
        assert!(request(&args).is_err());
    }
}
