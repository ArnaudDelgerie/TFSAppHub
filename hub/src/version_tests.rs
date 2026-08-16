use super::parse_app_version;

#[test]
fn accepts_zero_version() {
    assert_eq!(
        parse_app_version("0.0.0").expect("zero is canonical"),
        semver::Version::new(0, 0, 0)
    );
}

#[test]
fn accepts_three_component_version() {
    assert_eq!(
        parse_app_version("1.2.3").expect("three components are canonical"),
        semver::Version::new(1, 2, 3)
    );
}

#[test]
fn refuses_prefix() {
    assert_invalid("v1.2.3");
}

#[test]
fn refuses_missing_component() {
    assert_invalid("1.2");
}

#[test]
fn refuses_leading_zero() {
    assert_invalid("01.2.3");
}

#[test]
fn refuses_pre_release() {
    assert_invalid("1.2.3-rc.1");
}

#[test]
fn refuses_build_metadata() {
    assert_invalid("1.2.3+build.7");
}

#[test]
fn refuses_whitespace() {
    assert_invalid(" 1.2.3 ");
}

#[test]
fn refuses_malformed_semver() {
    assert_invalid("one.two.three");
}

#[test]
fn returned_versions_keep_semantic_ordering() {
    let older = parse_app_version("1.2.3").expect("canonical version");
    let newer = parse_app_version("1.2.4").expect("canonical version");

    assert!(older < newer);
}

fn assert_invalid(raw: &str) {
    let error = parse_app_version(raw).expect_err(&format!("{raw:?} is not canonical"));
    assert_eq!(
        error.to_string(),
        "must be canonical MAJOR.MINOR.PATCH, with no prefix, leading zeroes or suffix"
    );
}
