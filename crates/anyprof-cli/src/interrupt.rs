use std::io::Write;
use std::sync::{Arc, Mutex, Once};

use anyhow::Context;
use anyprof_core::{SignalAction, StopGate};

static CURRENT: Mutex<Option<Arc<StopGate>>> = Mutex::new(None);
static INSTALL: Once = Once::new();

/// Send Ctrl+C, SIGTERM, and Ctrl+Break to `gate` from now on.
///
/// The process-wide handler can only be installed once, so `record --web`
/// points it at the recording's gate first and the viewer's gate after.
pub fn route_to(gate: &Arc<StopGate>) -> anyhow::Result<()> {
    *CURRENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(gate));
    let mut installed = Ok(());
    INSTALL.call_once(|| {
        installed = ctrlc::set_handler(on_signal).context("installing the Ctrl+C handler");
    });
    installed
}

fn on_signal() {
    let gate = CURRENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let Some(gate) = gate else {
        return;
    };
    match gate.note_signal() {
        SignalAction::StopSampling => {}
        SignalAction::ForceExit => {
            eprintln!("\nanyprof: second interrupt, exiting immediately");
            let _ = std::io::stderr().flush();
            std::process::exit(130);
        }
    }
}
