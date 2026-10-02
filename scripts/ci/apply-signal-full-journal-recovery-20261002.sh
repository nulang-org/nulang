#!/usr/bin/env bash
set -euo pipefail

git config user.name 'nlc-ci'
git config user.email 'nlc-ci@users.noreply.github.com'

git fetch origin 0a05af596f44df22966b5664679b233848ef5474
git cherry-pick 0a05af596f44df22966b5664679b233848ef5474

python3 - <<'PY'
from pathlib import Path
p = Path('src/runtime/mod.rs')
text = p.read_text()
old = '''            for event in &all_timer_events {
                if let WorkflowEvent::TimerSet {
                    name, duration_ms, ..
                } = event
                {
                    if !fired_timer_names.contains(name) {
                        self.rearm_timer(actor_id, name, *duration_ms);
                    }
                }
            }
            // If the workflow was in the middle of a step waiting on a signal,
'''
new = '''            for event in &all_timer_events {
                if let WorkflowEvent::TimerSet {
                    name, duration_ms, ..
                } = event
                {
                    if !fired_timer_names.contains(name) {
                        self.rearm_timer(actor_id, name, *duration_ms);
                    }
                }
            }

            // Signal availability is not part of ActorSnapshot. Rebuild it from
            // the full signal journal even when a later completed snapshot has a
            // sequence at or beyond a SignalReceived record.
            let signal_events = self.persistence.read_signal_events(actor_id);
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                actor.received_signals.clear();
                for event in &signal_events {
                    if let WorkflowEvent::SignalReceived { name, payload, .. } = event {
                        actor.received_signals.push((name.clone(), payload.clone()));
                    }
                }
            }

            // If the workflow was in the middle of a step waiting on a signal,
'''
assert text.count(old) == 1, text.count(old)
p.write_text(text.replace(old, new))
PY

git diff --check
cargo test --locked --lib test_signal_received_survives_process_restart_from_libsql_journal -- --nocapture
cargo test --locked --lib test_signal_journal_survives_process_restart_when_snapshot_covers_signal -- --nocapture
cargo test --locked --lib signal_received_ -- --nocapture

git add src/runtime/mod.rs
git diff --cached --check
git commit -m 'fix(workflow): rebuild durable signals from full journal'
git push origin HEAD:fix/workflow-signal-full-journal-recovery-20261002
