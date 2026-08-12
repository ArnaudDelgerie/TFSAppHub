use super::*;

fn spec(source: Source) -> LaunchSpec {
    LaunchSpec {
        source,
        app_dir: PathBuf::from("/apps/demo"),
        identity: Identity {
            identifier: "dev.local.demo".to_string(),
            product_name: "Demo App".to_string(),
            icon_path: None,
        },
        manifest: crate::manifest::parse(
            std::path::Path::new("tfsapp.config.json"),
            r#"{
                "product_name": "Demo App",
                "identifier": "dev.local.demo",
                "project_name": "demo",
                "app_version": "0.6.0"
            }"#,
        )
        .expect("a valid manifest")
        .manifest,
        state_root: PathBuf::from("/data/dev.local.demo"),
        label: "demo".to_string(),
        warnings: vec![],
        update: update_check::Context::Dev,
        expected_cache: None,
    }
}

#[test]
fn installed_id_answers_the_handle() {
    let spec = spec(Source::Installed {
        id: "demo".to_string(),
    });
    assert_eq!(spec.installed_id(), Some("demo"));
}

#[test]
fn live_id_is_none() {
    let spec = spec(Source::Live);
    assert_eq!(spec.installed_id(), None);
}
