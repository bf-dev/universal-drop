use crate::service::{AppState, should_ignore_input_path};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use tracing::{debug, warn};

pub fn start_input_watcher(state: AppState) -> notify::Result<RecommendedWatcher> {
    let input_dir = state.config.input_dir.clone();
    let mut watcher = recommended_watcher(move |result: notify::Result<Event>| match result {
        Ok(event) => handle_event(&state, event),
        Err(error) => warn!(error = %error, "input watcher error"),
    })?;
    watcher.watch(&input_dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

fn handle_event(state: &AppState, event: Event) {
    if matches!(event.kind, EventKind::Remove(_) | EventKind::Access(_)) {
        return;
    }

    for path in event.paths {
        if should_ignore_input_path(&path) || !path.is_file() {
            continue;
        }
        match state.enqueue_path(path.clone()) {
            Ok(job) => {
                debug!(job_id = %job.id, path = %path.display(), "watcher queued input file")
            }
            Err(error) => {
                warn!(error = %error, path = %path.display(), "watcher failed to queue input file")
            }
        }
    }
}
