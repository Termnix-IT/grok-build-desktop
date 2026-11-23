//! Console entry point for Grok Bot: `gbd <command>` talks to the running Grok Build Desktop
//! over a named pipe. The desktop app itself is a GUI binary and cannot print to a console.

fn main() {
    std::process::exit(grok_build_desktop_lib::gbd_main());
}
