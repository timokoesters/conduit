use std::{
    fs::{File, create_dir_all},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::Result;
use conduit_config::rate_limiting::{
    AuthenticationFailures, ClientMediaConfig, ClientRestriction, Config, ConfigPreset,
    DocumentEnum, DocumentStruct, FederationMediaConfig, FederationRestriction,
    GetStructFieldValue,
};

fn docs_root() -> PathBuf {
    Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        // workspace root
        .nth(1)
        .unwrap()
        // docs root
        .join("target/docs")
        .to_path_buf()
}

pub fn run() -> Result<()> {
    let mut markdown_text = String::new();

    markdown_text.push_str("<!-- ANCHOR: presets -->\n");
    for (preset, comment) in ConfigPreset::variant_doc_comments() {
        markdown_text.push_str(&format!(
            "- `{}`: {}\n",
            variant_to_string(&preset),
            comment
        ));
    }
    markdown_text.push_str("<!-- ANCHOR_END: presets -->\n");

    macro_rules! push_request_restrictions {
        ($restriction_kind:ident) => {
            markdown_text.push_str(&format!("{}\n", $restriction_kind::container_doc_comment()));
            for (restriction, comment) in $restriction_kind::variant_doc_comments() {
                markdown_text.push_str(&format!(
                    "##### `{}`\n{}\n###### Defaults:\n| Preset | Global Timeframe | Global Burst Capacity | Target Timeframe | Target Burst Capacity |\n| --- | --- | --- | --- | --- |\n",
                    variant_to_string(&restriction),
                    comment
                ));

                for (preset, _) in ConfigPreset::variant_doc_comments() {
                    let preset_config = Config::get_preset(preset);
                    let global = preset_config.global.get(&restriction.into());
                    let target = preset_config.target.get(&restriction.into());
                    markdown_text.push_str(&format!(
                        "| `{}` | {} | {} requests | {} | {} requests |\n",
                        variant_to_string(&preset),
                        global.timeframe,
                        global.burst_capacity,
                        target.timeframe,
                        target.burst_capacity,
                    ))
                };
            }
        };
    }

    macro_rules! push_request_additional_fields {
        ($restriction_kind:ident, $api:ident, $scope:ident, $scope_str:expr) => {
            markdown_text.push_str(&format!("{}\n", $restriction_kind::container_doc_comment()));
            for (restriction, comment) in $restriction_kind::field_doc_comments() {
                markdown_text.push_str(&format!(
                    "##### `{}`\n{}\n###### Defaults:\n| Preset | {} Timeframe | {} Burst Capacity |\n| --- | --- | --- |\n",
                    &restriction,
                    comment,
                    $scope_str,
                    $scope_str,
                ));

                for (preset, _) in ConfigPreset::variant_doc_comments() {
                    let preset_config = Config::get_preset(preset);
                    let scope = preset_config.$scope.$api.additional_fields.get(&restriction).unwrap();
                    markdown_text.push_str(&format!(
                        "| `{}` | {} | {} requests |\n",
                        variant_to_string(&preset),
                        scope.timeframe,
                        scope.burst_capacity,
                    ))
                };
            }
        };
    }

    markdown_text.push_str("<!-- ANCHOR: client-request-restrictions -->\n");
    push_request_restrictions!(ClientRestriction);
    push_request_additional_fields!(AuthenticationFailures, client, target, "Target");

    markdown_text.push_str("<!-- ANCHOR_END: client-request-restrictions -->\n");

    markdown_text.push_str("<!-- ANCHOR: federation-request-restrictions -->\n");
    push_request_restrictions!(FederationRestriction);
    markdown_text.push_str("<!-- ANCHOR_END: federation-request-restrictions -->\n");

    macro_rules! push_media_restrictions {
        ($restriction_kind:ident, $api:ident) => {
            markdown_text.push_str(&format!("{}\n", $restriction_kind::container_doc_comment()));
            for (restriction, comment) in $restriction_kind::field_doc_comments() {
                markdown_text.push_str(&format!(
                    "##### `{}`\n{}\n###### Defaults:\n| Preset | Global Timeframe | Global Burst Capacity | Target Timeframe | Target Burst Capacity |\n| --- | --- | --- | --- | --- |\n",
                    &restriction,
                    comment
                ));

                for (preset, _) in ConfigPreset::variant_doc_comments() {
                    let preset_config = Config::get_preset(preset);
                    let global = preset_config.global.$api.media.get(&restriction).expect("For every restriction, we have a preset limitation configuration");
                    let target = preset_config.target.$api.media.get(&restriction).expect("For every restriction, we have a preset limitation configuration");
                    markdown_text.push_str(&format!(
                        "| `{}` | {} | {} | {} | {} |\n",
                        variant_to_string(&preset),
                        global.timeframe,
                        global.burst_capacity.display().si(),
                        target.timeframe,
                        target.burst_capacity.display().si(),
                    ))
                };
            }
        };
    }

    markdown_text.push_str("<!-- ANCHOR: client-media-restrictions -->\n");
    push_media_restrictions!(ClientMediaConfig, client);
    markdown_text.push_str("<!-- ANCHOR_END: client-media-restrictions -->\n");

    markdown_text.push_str("<!-- ANCHOR: federation-media-restrictions -->\n");
    push_media_restrictions!(FederationMediaConfig, federation);
    markdown_text.push_str("<!-- ANCHOR_END: federation-media-restrictions -->\n");

    create_dir_all("./target/docs")?;
    let mut file = File::create(docs_root().join("rate-limiting.md"))?;

    file.write_all(markdown_text.as_bytes())?;

    Ok(())
}

fn variant_to_string<T: serde::Serialize>(restriction: &T) -> String {
    // Maybe there is a better way to convert it to snake_case without extra dependencies added
    // to Cargo.lock
    serde_json::to_string(restriction)
        .expect("Unit variants can always serialize")
        .strip_prefix("\"")
        .expect("serde_json always adds this prefix")
        .strip_suffix("\"")
        .expect("serde_json always adds this suffix")
        .to_owned()
}
