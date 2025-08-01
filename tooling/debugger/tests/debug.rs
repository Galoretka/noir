#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    // Some of these imports are consumed by the injected tests
    use assert_cmd::cargo::cargo_bin;

    use rexpect::{session::PtyReplSession, spawn_bash}; // cSpell:disable-line

    // include tests generated by `build.rs`
    include!(concat!(env!("OUT_DIR"), "/debug.rs"));

    fn debugger_execution_success(test_program_dir: &str) {
        let nargo_bin =
            cargo_bin("nargo").into_os_string().into_string().expect("Cannot parse nargo path");

        let mut dbg_session = start_debug_session(&format!(
            "{nargo_bin} debug --program-dir {test_program_dir} --force-brillig --expression-width 3"
        ));

        // send continue which should run to the program to end
        // given we haven't set any breakpoints.
        send_continue_and_check_no_panic(&mut dbg_session);

        send_quit(&mut dbg_session);
        dbg_session
            .exp_regex(".*Circuit witness successfully solved.*")
            .expect("Expected circuit witness to be successfully solved.");

        exit(dbg_session);
    }

    fn debugger_test_success(test_program_dir: &str, test_name: &str) {
        let nargo_bin =
            cargo_bin("nargo").into_os_string().into_string().expect("Cannot parse nargo path");

        let mut dbg_session = start_debug_session(
            &(format!(
                "{nargo_bin} debug --program-dir {test_program_dir} --test-name {test_name} --force-brillig --expression-width 3"
            )),
        );

        // send continue which should run to the program to end
        // given we haven't set any breakpoints.
        send_continue_and_check_no_panic(&mut dbg_session);

        send_quit(&mut dbg_session);
        dbg_session
            .exp_regex(".*Testing .*\\.\\.\\. .*ok.*")
            .expect("Expected test to be successful");

        exit(dbg_session);
    }

    fn start_debug_session(command: &str) -> PtyReplSession {
        let timeout_seconds = 30;
        let mut dbg_session =
            spawn_bash(Some(timeout_seconds * 1000)).expect("Could not start bash session");

        // Start debugger and test that it loads for the given program.
        dbg_session.execute(command, ".*\\Starting debugger.*").expect("Could not start debugger");
        dbg_session
    }

    fn send_quit(dbg_session: &mut PtyReplSession) {
        // Run the "quit" command, then check that the debugger confirms
        // having successfully solved the circuit witness.
        dbg_session.send_line("quit").expect("Failed to quit debugger");
    }

    /// Exit the bash session.
    fn exit(mut dbg_session: PtyReplSession) {
        dbg_session.send_line("exit").expect("Failed to quit bash session");
    }

    /// While running the debugger, issue a "continue" cmd,
    /// which should run to the program to end or the next breakpoint
    /// ">" is the debugger's prompt, so finding one
    /// after running "continue" indicates that the
    /// debugger has not panicked.
    fn send_continue_and_check_no_panic(dbg_session: &mut PtyReplSession) {
        dbg_session
            .send_line("c")
            .expect("Debugger panicked while attempting to step through program.");
        dbg_session
            .exp_string(">")
            .expect("Failed while waiting for debugger to step through program.");
    }

    #[test]
    fn debugger_expected_call_stack() {
        let nargo_bin =
            cargo_bin("nargo").into_os_string().into_string().expect("Cannot parse nargo path");

        let timeout_seconds = 30;
        let mut dbg_session =
            spawn_bash(Some(timeout_seconds * 1000)).expect("Could not start bash session");

        let test_program_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test_programs/execution_success/regression_7195")
            .canonicalize()
            .unwrap();
        let test_program_dir = test_program_path.display();

        // Start debugger and test that it loads for the given program.
        dbg_session
            .execute(
                &format!(
                    "{nargo_bin} debug --raw-source-printing true --program-dir {test_program_dir} --force-brillig --expression-width 3"
                ),
                ".*\\Starting debugger.*",
            )
            .expect("Could not start debugger");

        let expected_lines_by_command: Vec<VecDeque<&str>> = vec![
            VecDeque::from(["fn main(x: Field, y: pub Field) {"]),
            VecDeque::from(["fn main(x: Field, y: pub Field) {"]),
            VecDeque::from(["fn main(x: Field, y: pub Field) {"]),
            VecDeque::from([
                "let x = unsafe { baz(x) };",
                "unconstrained fn baz(x: Field) -> Field {",
            ]),
            VecDeque::from([
                "let x = unsafe { baz(x) };",
                "unconstrained fn baz(x: Field) -> Field {",
            ]),
            VecDeque::from(["let x = unsafe { baz(x) };", "}"]),
            VecDeque::from(["let x = unsafe { baz(x) };"]),
            VecDeque::from(["foo(x);", "fn foo(x: Field) {"]),
            VecDeque::from(["foo(x);", "fn foo(x: Field) {"]),
            VecDeque::from([
                "foo(x);",
                "let y = unsafe { baz(x) };",
                "unconstrained fn baz(x: Field) -> Field {",
            ]),
            VecDeque::from([
                "foo(x);",
                "let y = unsafe { baz(x) };",
                "unconstrained fn baz(x: Field) -> Field {",
            ]),
            VecDeque::from(["foo(x);", "let y = unsafe { baz(x) };", "}"]),
            VecDeque::from(["foo(x);", "let y = unsafe { baz(x) };"]),
            VecDeque::from(["foo(x);", "bar(y);", "fn bar(y: Field) {"]),
            VecDeque::from(["foo(x);", "bar(y);", "fn bar(y: Field) {"]),
            VecDeque::from(["foo(x);", "bar(y);", "assert(y != 0);"]),
        ];

        // Try to use relative paths if possible, otherwise the test breaks when run from the repository root
        let at_filename = std::env::current_dir()
            .ok()
            .and_then(|dir| test_program_path.strip_prefix(&dir).ok())
            .map(|stripped| format!("At {}", stripped.display()))
            .unwrap_or_else(|| format!("At {test_program_dir}"));

        for mut expected_lines in expected_lines_by_command {
            // While running the debugger, issue a "next" cmd,
            // which should run to the program to the next source line given
            // we haven't set any breakpoints.
            // ">" is the debugger's prompt, so finding one
            // after running "next" indicates that the
            // debugger has not panicked for this step.
            dbg_session
                .send_line("next")
                .expect("Debugger panicked while attempting to step through program.");
            dbg_session
                .exp_string(">")
                .expect("Failed while waiting for debugger to step through program.");

            while let Some(expected_line) = expected_lines.pop_front() {
                let line = loop {
                    let read_line = dbg_session.read_line().unwrap();
                    if !(read_line.contains("> next")
                        || read_line.contains("At opcode")
                        || read_line.contains(at_filename.as_str())
                        || read_line.contains("..."))
                    {
                        break read_line;
                    }
                };
                let ascii_line: String = line.chars().filter(char::is_ascii).collect();
                let line_expected_to_contain = expected_line.trim();
                assert!(
                    ascii_line.contains(line_expected_to_contain),
                    "{ascii_line:?}\ndid not contain\n{line_expected_to_contain:?}",
                );
            }
        }

        // Run the "quit" command
        dbg_session.send_line("quit").expect("Failed to quit debugger");

        // Exit the bash session.
        dbg_session.send_line("exit").expect("Failed to quit bash session");
    }
}
