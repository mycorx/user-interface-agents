// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

// The release workflow installs the built .msi / .deb on a clean machine and
// runs the installed binary as `uia --smoke`. That proves the one thing a
// build on the developer's machine cannot: that the shipped binary starts and
// links on a machine with nothing else installed (a missing shared library
// kills it before `main` runs, so reaching `main` at all is the test).
//
// uia is a GUI app and, in a release build on Windows, has no console
// (`windows_subsystem = "windows"`), so there is nothing to print and the
// answer is the exit code. The decision is a pure function so it is tested
// here, in the sandbox where `main.rs` cannot even be compiled.

/// The flag the smoke test passes. Not a user feature, and not documented as one.
pub const SMOKE_FLAG: &str = "--smoke";

/// Whether this process was started as a smoke test: the flag must be the
/// first argument after the program name, and nothing else.
pub fn is_smoke_invocation<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter();
    args.next(); // argv[0], the program itself
    matches!(args.next(), Some(first) if first.as_ref() == SMOKE_FLAG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_after_the_program_name_is_a_smoke_run() {
        assert!(is_smoke_invocation(["uia", "--smoke"]));
    }

    #[test]
    fn a_plain_launch_is_not() {
        assert!(!is_smoke_invocation(["uia"]));
        assert!(!is_smoke_invocation(Vec::<String>::new()));
    }

    #[test]
    fn the_flag_as_the_program_name_does_not_count() {
        // argv[0] is whatever launched us; it must never be read as a flag.
        assert!(!is_smoke_invocation(["--smoke"]));
    }

    #[test]
    fn near_misses_and_later_positions_are_not() {
        assert!(!is_smoke_invocation(["uia", "--smoke-extra"]));
        assert!(!is_smoke_invocation(["uia", "--SMOKE"]));
        assert!(!is_smoke_invocation(["uia", "settings", "--smoke"]));
    }
}
