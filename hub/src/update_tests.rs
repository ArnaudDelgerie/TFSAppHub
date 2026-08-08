use super::{check, UpdateCheckResult, HOST_RESOLVES_UPDATES};

#[test]
fn the_hub_answers_unavailable_rather_than_failing() {
    // A check that cannot be made is a result, not an error: an app has to be
    // able to call this on a timer without handling exceptions.
    assert_eq!(
        check(),
        UpdateCheckResult::Unavailable {
            reason: HOST_RESOLVES_UPDATES.to_string()
        }
    );
}

#[test]
fn the_answer_is_the_shape_the_station_already_serves() {
    let json = serde_json::to_value(check()).expect("it serialises");

    // Internally tagged on `status`, with no extra nesting — an app written
    // against the station's answers reads this one without a change, which is
    // what keeps it from having to know which host started it.
    assert_eq!(json["status"], "unavailable");
    assert_eq!(json["reason"], HOST_RESOLVES_UPDATES);
    assert_eq!(
        json.as_object().expect("an object").len(),
        2,
        "no field beyond status and reason: {json}"
    );
}
