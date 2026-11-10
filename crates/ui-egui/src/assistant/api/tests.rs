use super::*;

fn message(text: &str) -> Value {
    json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}]})
}
fn function(args: &str) -> Value {
    json!({"status":"completed","output":[
        {"type":"reasoning","id":"r1","summary":[],"encrypted_content":"opaque"},
        {"type":"function_call","id":"f1","call_id":"c1","name":"filmcraft","arguments":args}
    ]})
}

#[test]
fn responses_round_trip_preserves_reasoning_call_ids_and_image_results() {
    let mut requests = Vec::new();
    let mut calls = 0;
    let frame = json!([{"type":"input_text","text":"Program frame 0"},{"type":"input_image","image_url":"data:image/png;base64,test","detail":"auto"}]);
    let done = run(
        DEFAULT_MODEL,
        vec![json!({"role":"user","content":"inspect shot"})],
        &AtomicBool::new(false),
        |body| {
            requests.push(body.clone());
            if requests.len() == 1 { Ok(function(r#"{"method":"engine.commands","params_json":"{}"}"#)) } else { Ok(message("I inspected the shot.")) }
        },
        |name, args| {
            calls += 1;
            assert_eq!(name, "filmcraft");
            assert_eq!(args["method"], "engine.commands");
            Ok(frame.clone())
        },
    )
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(done.text, "I inspected the shot.");
    assert_eq!(requests[0]["store"], false);
    assert_eq!(requests[0]["parallel_tool_calls"], false);
    assert_eq!(requests[1]["input"][1]["encrypted_content"], "opaque");
    assert_eq!(requests[1]["input"][3]["call_id"], "c1");
    assert_eq!(requests[1]["input"][3]["output"], frame);
    let second = run(
        DEFAULT_MODEL,
        done.history,
        &AtomicBool::new(false),
        |body| {
            assert!(body["input"].as_array().unwrap().iter().any(|i| i["type"] == "function_call_output"));
            Ok(message("Still here."))
        },
        |_, _| panic!("No tool call expected"),
    )
    .unwrap();
    assert_eq!(second.text, "Still here.");
}

#[test]
fn malformed_arguments_return_tool_error_without_executing() {
    let mut n = 0;
    run(
        DEFAULT_MODEL,
        vec![],
        &AtomicBool::new(false),
        |body| {
            n += 1;
            if n == 1 {
                Ok(function("{bad"))
            } else {
                assert!(body["input"].as_array().unwrap().last().unwrap()["output"].as_str().unwrap().contains("JSON object"));
                Ok(message("The call was invalid."))
            }
        },
        |_, _| panic!("Malformed arguments must not execute"),
    )
    .unwrap();
}

#[test]
fn cancellation_between_api_and_tools_prevents_mutation() {
    let cancel = AtomicBool::new(false);
    let result = run(
        DEFAULT_MODEL,
        vec![],
        &cancel,
        |_| {
            cancel.store(true, Ordering::Release);
            Ok(function(r#"{"method":"engine.execute","params_json":"{}"}"#))
        },
        |_, _| panic!("Cancelled tool must not execute"),
    );
    assert!(result.err().unwrap().contains("Stopped"));
}

#[test]
fn runaway_tool_loop_is_bounded() {
    let mut calls = 0;
    let result = run(
        DEFAULT_MODEL,
        vec![],
        &AtomicBool::new(false),
        |_| Ok(function("{}")),
        |_, _| {
            calls += 1;
            Ok(Value::String("done".into()))
        },
    );
    assert_eq!(calls, MAX_CALLS);
    assert!(result.err().unwrap().contains("limit"));
}

#[test]
fn incomplete_or_hostile_responses_never_dispatch_tools() {
    for response in [Value::Null, json!({"status":"failed","error":{"message":"secret"}}), json!({"status":"completed","output":{}})] {
        assert!(run(DEFAULT_MODEL, vec![], &AtomicBool::new(false), |_| Ok(response.clone()), |_, _| panic!("Invalid response")).is_err());
    }
    assert!(!http_error(ureq::Error::StatusCode(401)).contains("secret"));
    for key in ["", "sk-bad\ncredential", &"a".repeat(513)] {
        assert!(http(key.into()).is_err());
    }
}
