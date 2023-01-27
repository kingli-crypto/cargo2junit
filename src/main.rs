extern crate junit_report;
extern crate serde;

use std::borrow::Cow;
use std::collections::HashMap;
use std::env;
use std::io::*;

use junit_report::*;
use serde::{Deserialize, Serialize};

const SYSTEM_OUT_MAX_LEN: usize = 65536;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct SuiteResults {
    passed: usize,
    failed: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct TestCaseDetail {
    start_time: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
enum DurationPrecision {
    MilliSeconds,
    LiteralSeconds
}

impl DurationPrecision {
    pub fn trunc(&self, duration : Duration) -> Duration {
        match &self {
            Self::MilliSeconds => Duration::microseconds(duration.num_microseconds().unwrap_or(0)),
            Self::LiteralSeconds => Duration::seconds(duration.num_seconds()),
        }
    }
}

impl TestCaseDetail {
    pub fn get_duration(&self, now: DateTime<Utc>) -> Duration {
        now - self.start_time
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "event")]
enum SuiteEvent {
    #[serde(rename = "started")]
    Started { test_count: usize },
    #[serde(rename = "ok")]
    Ok {
        #[serde(flatten)]
        results: SuiteResults,
    },
    #[serde(rename = "failed")]
    Failed {
        #[serde(flatten)]
        results: SuiteResults,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "event")]
enum TestEvent {
    #[serde(rename = "started")]
    Started { name: String },
    #[serde(rename = "ok")]
    Ok { name: String },
    #[serde(rename = "failed")]
    Failed {
        name: String,
        stdout: Option<String>,
        stderr: Option<String>,
    },
    #[serde(rename = "ignored")]
    Ignored { name: String },
    #[serde(rename = "timeout")]
    Timeout { name: String },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(untagged)]
enum Event {
    #[serde(rename = "suite")]
    Suite {
        #[serde(flatten)]
        event: SuiteEvent,
    },
    #[serde(rename = "test")]
    TestStringTime {
        #[serde(flatten)]
        event: TestEvent,
        duration: Option<f64>,
        exec_time: Option<String>,
    },
    #[serde(rename = "test")]
    TestFloatTime {
        #[serde(flatten)]
        event: TestEvent,
        duration: Option<f64>,
        exec_time: Option<f64>,
    },
}

impl Event {
    fn get_duration(&self) -> Option<Duration> {
        match &self {
            Event::Suite { event: _ } => panic!(),
            Event::TestStringTime {
                event: _,
                duration,
                exec_time,
            } => {
                let duration_ns = match (duration, exec_time) {
                    (_, Some(s)) => {
                        assert_eq!(s.chars().last(), Some('s'));
                        let seconds_chars = &(s[0..(s.len() - 1)]);
                        let seconds = seconds_chars.parse::<f64>().unwrap();
                        Some((seconds * 1_000_000_000.0) as i64)
                    }
                    (Some(ms), None) => Some((ms * 1_000_000.0) as i64),
                    (None, None) => None,
                };

                match duration_ns {
                    Some(duration) => Some(Duration::nanoseconds(duration)),
                    None => None,
                }
            }
            Event::TestFloatTime {
                event: _,
                duration,
                exec_time,
            } => {
                let duration_ns = match (duration, exec_time) {
                    (_, Some(seconds)) => Some((seconds * 1_000_000_000.0) as i64),
                    (Some(ms), None) => Some((ms * 1_000_000.0) as i64),
                    (None, None) => None,
                };

                match duration_ns {
                    Some(duration) => Some(Duration::nanoseconds(duration)),
                    None => None,
                }
            }
        }
    }
}

fn split_name(full_name: &str) -> (&str, String) {
    let mut parts: Vec<&str> = full_name.split("::").collect();
    let name = parts.pop().unwrap_or("");
    let module_path = parts.join("::");
    (name, module_path)
}

/// Attempt to populate failure with meaningful error messages
/// If stderr is valid / non trivial, use that
/// Otherwise attempt to extract error from stdout with regex
fn detect_error(stdout: &Option<String>, stderr: &Option<String>) -> Option<String> {
    if let Some(body) = stderr {
        if !body.trim().is_empty() {
            return Some(body.to_string());
        }
    }

    // guess
    let exp = regex::RegexBuilder::new(r"[\n^]([Ee]rror: .+)$")
        .build()
        .unwrap();
    if let Some(stdout) = stdout {
        if let Some(body) = exp.find(stdout) {
            return Some(body.as_str().trim().to_string());
        }
    }

    None
}

fn parse<T: BufRead>(
    input: T,
    suite_name_prefix: &str,
    timestamp: OffsetDateTime,
    max_out_len: usize,
    precision: DurationPrecision
) -> Result<Report> {
    let mut r = Report::new();
    let mut suite_index = 0;
    let mut current_suite_maybe: Option<TestSuite> = None;
    let mut tests: HashMap<String, TestCaseDetail> = HashMap::new();

    for line in input.lines() {
        let line = line?;

        if line.chars().find(|c| !c.is_whitespace()) != Some('{') {
            continue;
        }

        // println!("'{}'", &line);
        let e: Event = match serde_json::from_str(&line) {
            Ok(event) => Ok(event),
            Err(orig_err) => {
                // cargo test doesn't escape backslashes to do it ourselves and retry
                let line = line.replace("\\", "\\\\");
                match serde_json::from_str(&line) {
                    Ok(event) => Ok(event),
                    Err(_) => Err(Error::new(
                        ErrorKind::Other,
                        format!("Error parsing '{}': {}", &line, orig_err),
                    )),
                }
            }
        }?;

        // println!("{:?}", e);
        match &e {
            Event::Suite { event } => match event {
                SuiteEvent::Started { test_count: _ } => {
                    assert!(current_suite_maybe.is_none());
                    assert!(tests.is_empty());
                    let mut ts = TestSuite::new(&format!("{} #{}", suite_name_prefix, suite_index));
                    ts.set_timestamp(timestamp);
                    current_suite_maybe = Some(ts);
                    suite_index += 1;
                }
                SuiteEvent::Ok { results: _ } | SuiteEvent::Failed { results: _ } => {
                    assert_eq!(None, tests.iter().next());
                    r.add_testsuite(
                        current_suite_maybe.expect("Suite complete event found outside of suite!"),
                    );
                    current_suite_maybe = None;
                }
            },
            Event::TestStringTime {
                event,
                duration: _,
                exec_time: _,
            }
            | Event::TestFloatTime {
                event,
                duration: _,
                exec_time: _,
            } => {
                let mut current_suite = current_suite_maybe
                    .take()
                    .expect("Test event found outside of suite!");

                let duration = e.get_duration();

                match event {
                    TestEvent::Started { name } => {
                        assert!(tests
                            .insert(
                                name.clone(),
                                TestCaseDetail {
                                    start_time: Utc::now()
                                }
                            )
                            .is_none());
                    }
                    TestEvent::Ok { name } => {
                        let now = Utc::now();
                        let detail = tests.remove(name).unwrap();

                        let (name, module_path) = split_name(&name);
                        let mut tc = TestCase::success(&name, duration);
                        tc.set_classname(module_path.as_str());
                        current_suite.add_testcase(tc);
                    }
                    TestEvent::Failed {
                        name,
                        stdout,
                        stderr,
                    } => {
                        let now = Utc::now();
                        let detail = tests.remove(name).unwrap();

                        let (name, module_path) = split_name(&name);
                        let error_message = detect_error(stdout, stderr);

                        let mut failure = TestCase::failure(
                            &name,
                            precision.trunc(duration.unwrap_or(detail.get_duration(now))),
                            "cargo test",
                            &format!("failed {}::{}", module_path.as_str(), &name),
                        );
                        failure.set_classname(module_path.as_str());

                        fn truncate(s: &str, max_len: usize) -> Cow<'_, str> {
                            if s.len() > max_len {
                                let truncated_msg = "[...TRUNCATED...]";
                                let half_max_len = (max_len - truncated_msg.len()) / 2;
                                Cow::Owned(format!(
                                    "{}\n{}\n{}",
                                    s.split_at(half_max_len).0,
                                    truncated_msg,
                                    s.split_at(s.len() - half_max_len).1
                                ))
                            } else {
                                Cow::Borrowed(s)
                            }
                        }

                        // if a error message can be guessed, use that
                        if let Some(message) = error_message {
                            failure.set_system_out(&truncate(&message, max_out_len));
                        } else {
                            if let Some(stdout) = stdout {
                                failure.set_system_out(&truncate(stdout, max_out_len));
                            }

                            if let Some(stderr) = stderr {
                                failure.set_system_err(&truncate(stderr, max_out_len));
                            }
                        }

                        current_suite.add_testcase(failure);
                    }
                    TestEvent::Ignored { name } => {
                        assert!(tests.remove(name));
                        current_suite.add_testcase(TestCase::skipped(name));
                    }
                    TestEvent::Timeout { name: _ } => {
                        // An informative timeout event is emitted after a test has been running for
                        // 60 seconds. The test is not stopped, but continues running and will
                        // return its result at a later point in time.
                        // This event should be safe to ignore for now, but might require further
                        // action if hard timeouts that cancel and fail the test should be specified
                        // during or before stabilization of the JSON format.
                    }
                }

                current_suite_maybe = Some(current_suite);
            }
        }
    }

    Ok(r)
}

fn main() -> Result<()> {
    let timestamp = OffsetDateTime::now_utc();
    let stdin = std::io::stdin();
    let stdin = stdin.lock();

    // GitLab fails to parse the Junit XML if stdout is too long.
    let max_out_len = match env::var("TEST_STDOUT_STDERR_MAX_LEN") {
        Ok(val) => val
            .parse::<usize>()
            .expect("Failed to parse TEST_STDOUT_STDERR_MAX_LEN as a natural number"),
        Err(_) => SYSTEM_OUT_MAX_LEN,
    };
    let report = parse(stdin, "cargo test", timestamp, max_out_len, DurationPrecision::MilliSeconds)?;

    let stdout = std::io::stdout();
    let stdout = stdout.lock();
    report
        .write_xml(stdout)
        .map_err(|e| Error::new(ErrorKind::Other, format!("{:#}", e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::*;

    use junit_report::*;
    use regex::Regex;

    use crate::{DurationPrecision, parse};

    use super::SYSTEM_OUT_MAX_LEN;

    fn parse_bytes(bytes: &[u8], max_stdout_len: usize) -> Result<Report> {
        parse(
            BufReader::new(bytes),
            "cargo test",
            OffsetDateTime::now_utc(),
            max_stdout_len,
            DurationPrecision::LiteralSeconds
        )
    }

    fn parse_bytes_milli(bytes: &[u8], max_stdout_len: usize) -> Result<Report> {
        parse(
            BufReader::new(bytes),
            "cargo test",
            Utc::now(),
            max_stdout_len,
            DurationPrecision::MilliSeconds
        )
    }

    fn parse_string(input: &str, max_stdout_len: usize) -> Result<Report> {
        parse_bytes(input.as_bytes(), max_stdout_len)
    }

    fn normalize(input: &str) -> String {
        let date_regex =
            Regex::new(r"(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})\.(\d+)Z").unwrap();
        date_regex
            .replace_all(input, "TIMESTAMP")
            .replace("\r\n", "\n")
    }

    fn assert_output(report: &Report, expected: &[u8]) {
        let mut output = Vec::new();
        report.write_xml(&mut output).unwrap();
        let output = normalize(std::str::from_utf8(&output).unwrap());
        let expected = normalize(std::str::from_utf8(expected).unwrap());
        assert_eq!(output, expected);
    }

    #[test]
    fn error_on_garbage() {
        assert!(parse_string("{garbage}", SYSTEM_OUT_MAX_LEN).is_err());
    }

    #[test]
    fn success_self() {
        let report = parse_bytes_milli(include_bytes!("test_inputs/self.json"), SYSTEM_OUT_MAX_LEN)
            .expect("Could not parse test input");
        let suite = &report.testsuites()[0];
        let test_cases = suite.testcases();
        assert_eq!(test_cases[0].name(), "error_on_garbage");
        assert_eq!(*test_cases[0].classname(), Some("tests".to_string()));
        assert_eq!(test_cases[0].time(), &Duration::nanoseconds(213_000));

        assert_output(&report, include_bytes!("expected_outputs/self.out"));
    }

    #[test]
    fn success_self_exec_time() {
        let report = parse_bytes_milli(
            include_bytes!("test_inputs/self_exec_time.json"),
            SYSTEM_OUT_MAX_LEN,
        )
        .expect("Could not parse test input");
        let suite = &report.testsuites()[0];
        let test_cases = suite.testcases();
        assert_eq!(test_cases[4].name(), "az_func_regression");
        assert_eq!(*test_cases[0].classname(), Some("tests".to_string()));
        assert_eq!(test_cases[4].time(), &Duration::milliseconds(72));
        assert_output(
            &report,
            include_bytes!("expected_outputs/self_exec_time.out"),
        );
    }

    #[test]
    fn success_single_suite() {
        let report = parse_bytes(
            include_bytes!("test_inputs/success.json"),
            SYSTEM_OUT_MAX_LEN,
        )
        .expect("Could not parse test input");
        assert_output(&report, include_bytes!("expected_outputs/success.out"));
    }

    #[test]
    fn timedout_event() {
        let report = parse_bytes(
            include_bytes!("test_inputs/timed_out_event.json"),
            SYSTEM_OUT_MAX_LEN,
        )
        .expect("Could not parse test input");

        let suite = &report.testsuites()[0];
        let test_cases = suite.testcases();
        assert_eq!(test_cases[0].name(), "long_execution_time");
        assert_eq!(*test_cases[0].classname(), Some("tests".to_string()));
        assert!(test_cases[0].is_success());
        assert_output(
            &report,
            include_bytes!("expected_outputs/timed_out_event.out"),
        );
    }

    #[test]
    fn single_suite_failed() {
        let report = parse_bytes(
            include_bytes!("test_inputs/failed.json"),
            SYSTEM_OUT_MAX_LEN,
        )
        .expect("Could not parse test input");
        assert_output(&report, include_bytes!("expected_outputs/failed.out"));
    }

    #[test]
    fn single_suite_failed_guessed() {
        let report = parse_bytes(
            include_bytes!("test_inputs/failed_guessed.json"),
            SYSTEM_OUT_MAX_LEN,
        )
        .expect("Could not parse test input");
        assert_output(
            &report,
            include_bytes!("expected_outputs/failed_guessed.out"),
        );
    }

    #[test]
    fn single_suite_failed_stderr_only() {
        let report = parse_bytes(
            include_bytes!("test_inputs/failed_stderr.json"),
            SYSTEM_OUT_MAX_LEN,
        )
        .expect("Could not parse test input");
        assert_output(
            &report,
            include_bytes!("expected_outputs/failed_stderr.out"),
        );
    }

    #[test]
    fn multi_suite_success() {
        let report = parse_bytes(
            include_bytes!("test_inputs/multi_suite_success.json"),
            SYSTEM_OUT_MAX_LEN,
        )
        .expect("Could not parse test input");
        assert_output(
            &report,
            include_bytes!("expected_outputs/multi_suite_success.out"),
        );
    }

    #[test]
    fn cargo_project_failure() {
        let report = parse_bytes(
            include_bytes!("test_inputs/cargo_failure.json"),
            SYSTEM_OUT_MAX_LEN,
        )
        .expect("Could not parse test input");
        assert_output(
            &report,
            include_bytes!("expected_outputs/cargo_failure.out"),
        );
    }

    #[test]
    fn cargo_project_failure_shortened() {
        let report = parse_bytes(include_bytes!("test_inputs/cargo_failure.json"), 256)
            .expect("Could not parse test input");
        assert_output(
            &report,
            include_bytes!("expected_outputs/cargo_failure_shortened.out"),
        );
    }

    #[test]
    fn az_func_regression() {
        let report = parse_bytes(
            include_bytes!("test_inputs/azfunc.json"),
            SYSTEM_OUT_MAX_LEN,
        )
        .expect("Could not parse test input");
        assert_output(&report, include_bytes!("expected_outputs/azfunc.out"));
    }

    #[test]
    fn float_time() {
        parse_bytes(
            include_bytes!("test_inputs/float_time.json"),
            SYSTEM_OUT_MAX_LEN,
        )
        .expect("Could not parse test input");
    }
}
