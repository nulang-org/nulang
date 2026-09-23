//! Local agent runtime: Director + Manager + Worker + SQLite + NLAP events.

use crate::config::AgentConfigFile;
use crate::store::{
    AgentResumeTransition, AgentStateTransition, ResumeCommitResult, SqliteStore, StoreError,
};
use chrono::Utc;
use nulang_ai_core::{
    Commitment, CommitmentResumption, CommitmentStatus, ConversationMessage, ConversationState,
    Goal, GoalStatus, Intention, IntentionRevision, IntentionRevisionDecision, IntentionStatus,
    SwarmEvent, SwarmEventEnvelope, Task, TaskStatus,
};
use nulang_ai_director::{Director, LocalDirector};
use nulang_ai_manager::{EngineeringManager, Manager};
use nulang_ai_protocol::format_event_line;
use nulang_ai_worker::{LocalWorker, Worker};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_AUTOMATIC_REPLANS: usize = 1;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("missing agent.toml in {0}")]
    MissingConfig(PathBuf),
}

enum PlanOutcome {
    Completed {
        terminal_task: Option<Task>,
        agent_id: Option<String>,
    },
    Blocked {
        terminal_task: Task,
        agent_id: String,
        reason: String,
    },
    Failed {
        terminal_task: Task,
        agent_id: String,
        reason: String,
    },
}

pub struct LocalRuntime {
    project_dir: PathBuf,
    config: AgentConfigFile,
    store: SqliteStore,
    conversation_id: Uuid,
    project_id: String,
    director: LocalDirector,
    engineering: EngineeringManager,
    worker: Box<dyn Worker>,
}

impl LocalRuntime {
    pub fn open(project_dir: PathBuf) -> Result<Self, RuntimeError> {
        Self::open_with_worker(project_dir, Box::new(LocalWorker::new("worker-local")))
    }

    pub fn open_with_worker(
        project_dir: PathBuf,
        worker: Box<dyn Worker>,
    ) -> Result<Self, RuntimeError> {
        if !project_dir.join("agent.toml").exists() {
            return Err(RuntimeError::MissingConfig(project_dir));
        }
        let config = AgentConfigFile::load(&project_dir)?;
        let data_dir = config.resolve_data_dir(&project_dir);
        let store = SqliteStore::open(&data_dir)?;
        let project_id = project_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("default")
            .to_string();
        let conversation_id = Uuid::new_v4();
        let now = Utc::now();
        let conv = ConversationState {
            id: conversation_id,
            project_id: project_id.clone(),
            director_id: Some("director-local".into()),
            active_goal_id: None,
            messages: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        store.upsert_conversation(&conv)?;
        Ok(Self {
            project_dir,
            config,
            store,
            conversation_id,
            project_id,
            director: LocalDirector::new("director-local"),
            engineering: EngineeringManager,
            worker,
        })
    }

    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    pub fn conversation_id(&self) -> Uuid {
        self.conversation_id
    }

    pub fn handle_user_message(
        &mut self,
        text: &str,
        out: &mut dyn Write,
    ) -> Result<Uuid, RuntimeError> {
        self.flush_pending_events(out)?;

        let now = Utc::now();
        let mut conv = self.store.get_conversation(self.conversation_id)?;
        conv.messages.push(ConversationMessage {
            role: "user".into(),
            content: text.into(),
            timestamp: now,
        });
        conv.updated_at = now;
        self.store.upsert_conversation(&conv)?;

        let mut goal = self.director.create_goal(
            &self.project_id,
            self.conversation_id,
            text,
            self.config.director.default_budget_usd,
        );
        let goal_id = goal.id;
        self.store.upsert_goal(&goal)?;
        self.emit(
            out,
            SwarmEvent::GoalCreated {
                goal_id,
                conversation_id: Some(self.conversation_id),
            },
        )?;

        goal.status = GoalStatus::Running;
        goal.updated_at = Utc::now();
        self.store.upsert_goal(&goal)?;

        let mut commitment = Commitment::new(
            goal_id,
            "director-local",
            "User goal accepted for execution",
        );
        commitment.success_criteria = goal.success_criteria.clone();
        commitment.status = CommitmentStatus::Active;
        commitment.updated_at = Utc::now();
        self.store.upsert_commitment(&commitment)?;
        self.emit(
            out,
            SwarmEvent::CommitmentActivated {
                commitment_id: commitment.id,
                goal_id,
                owner_agent_id: commitment.owner_agent_id.clone(),
            },
        )?;

        conv.active_goal_id = Some(goal_id);
        conv.updated_at = Utc::now();
        self.store.upsert_conversation(&conv)?;

        let tasks =
            self.engineering
                .plan_tasks(goal_id, text, self.config.director.default_budget_usd);
        let intention = self.new_intention(goal_id, commitment.id, 0, &tasks);
        self.activate_intention(&intention, out)?;

        self.drive_goal(
            goal,
            commitment,
            intention,
            tasks,
            text,
            Some(self.conversation_id),
            out,
            true,
            0,
        )
    }

    pub fn resume_goal(
        &mut self,
        goal_id: Uuid,
        request_id: Uuid,
        reason: &str,
        out: &mut dyn Write,
    ) -> Result<Uuid, RuntimeError> {
        self.flush_pending_events(out)?;

        if let Some(existing) = self.store.get_resumption_by_request(request_id)? {
            if existing.goal_id != goal_id {
                return Err(StoreError::InvalidTransition(
                    "resume request id already belongs to a different goal",
                )
                .into());
            }
            return self.continue_existing_resumption(existing, out);
        }

        let graph = self.store.get_goal_graph(goal_id)?;
        if graph.goal.status != GoalStatus::Blocked {
            return Err(StoreError::InvalidTransition("goal is not blocked").into());
        }

        let blocked_commitment = graph
            .commitments
            .iter()
            .rev()
            .find(|commitment| commitment.status == CommitmentStatus::Suspended)
            .cloned()
            .ok_or(StoreError::InvalidTransition(
                "blocked goal has no suspended commitment",
            ))?;
        let blocked_intention = graph
            .intentions
            .iter()
            .rev()
            .find(|intention| {
                intention.commitment_id == blocked_commitment.id
                    && intention.status == IntentionStatus::Blocked
            })
            .cloned()
            .ok_or(StoreError::InvalidTransition(
                "suspended commitment has no blocked intention",
            ))?;

        let mut goal = graph.goal.clone();
        let mut commitment = blocked_commitment;
        let tasks = self.engineering.plan_tasks(
            goal_id,
            &goal.intent,
            self.config.director.default_budget_usd,
        );
        let replacement =
            self.new_intention(goal_id, commitment.id, graph.intentions.len(), &tasks);
        let resumption =
            CommitmentResumption::new(request_id, &blocked_intention, replacement.id, reason);

        commitment.status = CommitmentStatus::Active;
        commitment.updated_at = Utc::now();
        goal.status = GoalStatus::Running;
        goal.updated_at = Utc::now();

        let conversation_id = goal.conversation_id;
        let mut outbox_events = vec![
            self.envelope_for(
                SwarmEvent::CommitmentResumed {
                    commitment_id: commitment.id,
                    goal_id,
                    request_id,
                    reason: reason.to_string(),
                },
                conversation_id,
            ),
            self.envelope_for(
                SwarmEvent::GoalResumed {
                    goal_id,
                    request_id,
                    reason: reason.to_string(),
                },
                conversation_id,
            ),
            self.envelope_for(
                SwarmEvent::IntentionActivated {
                    intention_id: replacement.id,
                    commitment_id: replacement.commitment_id,
                    goal_id: replacement.goal_id,
                    owner_agent_id: replacement.owner_agent_id.clone(),
                },
                conversation_id,
            ),
        ];
        for task in &tasks {
            outbox_events.push(self.envelope_for(
                SwarmEvent::TaskCreated {
                    task_id: task.id,
                    goal_id: task.goal_id,
                },
                conversation_id,
            ));
        }

        match self
            .store
            .commit_agent_resume_transition(AgentResumeTransition {
                blocked_intention: &blocked_intention,
                replacement_intention: &replacement,
                replacement_tasks: &tasks,
                resumption: &resumption,
                commitment: &commitment,
                goal: &goal,
                outbox_events: &outbox_events,
            })? {
            ResumeCommitResult::Applied(_) => {
                self.flush_pending_events(out)?;
                let plan_text = goal.intent.clone();
                self.drive_goal(
                    goal,
                    commitment,
                    replacement,
                    tasks,
                    &plan_text,
                    conversation_id,
                    out,
                    false,
                    0,
                )
            }
            ResumeCommitResult::AlreadyApplied(existing) => {
                self.flush_pending_events(out)?;
                self.continue_existing_resumption(existing, out)
            }
        }
    }

    fn continue_existing_resumption(
        &mut self,
        resumption: CommitmentResumption,
        out: &mut dyn Write,
    ) -> Result<Uuid, RuntimeError> {
        let graph = self.store.get_goal_graph(resumption.goal_id)?;
        if !graph
            .intentions
            .iter()
            .any(|intention| intention.id == resumption.replacement_intention_id)
        {
            return Err(StoreError::InvalidTransition(
                "resumption replacement intention is missing",
            )
            .into());
        }

        if graph.goal.status != GoalStatus::Running {
            self.flush_pending_events(out)?;
            return Ok(resumption.goal_id);
        }

        let replacement = graph
            .intentions
            .iter()
            .rev()
            .find(|intention| {
                intention.commitment_id == resumption.commitment_id
                    && intention.status == IntentionStatus::Active
            })
            .cloned()
            .ok_or(StoreError::InvalidTransition(
                "running resumed goal has no active intention",
            ))?;

        let commitment = graph
            .commitments
            .iter()
            .find(|commitment| commitment.id == resumption.commitment_id)
            .cloned()
            .ok_or(StoreError::InvalidTransition(
                "resumption commitment is missing",
            ))?;
        if commitment.status != CommitmentStatus::Active {
            return Err(
                StoreError::InvalidTransition("resumption commitment is not active").into(),
            );
        }

        let tasks = ordered_tasks_for_intention(&graph.tasks, &replacement)?;
        let goal = graph.goal;
        let conversation_id = goal.conversation_id;
        let plan_text = goal.intent.clone();
        self.drive_goal(
            goal,
            commitment,
            replacement,
            tasks,
            &plan_text,
            conversation_id,
            out,
            false,
            0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn drive_goal(
        &mut self,
        mut goal: Goal,
        mut commitment: Commitment,
        mut intention: Intention,
        mut tasks: Vec<Task>,
        plan_text: &str,
        conversation_id: Option<Uuid>,
        out: &mut dyn Write,
        mut initialize_tasks: bool,
        mut replan_count: usize,
    ) -> Result<Uuid, RuntimeError> {
        let goal_id = goal.id;

        loop {
            match self.execute_intention(
                &mut intention,
                tasks,
                out,
                initialize_tasks,
                conversation_id,
            )? {
                PlanOutcome::Completed {
                    terminal_task,
                    agent_id,
                } => {
                    commitment.status = CommitmentStatus::Fulfilled;
                    commitment.updated_at = Utc::now();
                    goal.status = GoalStatus::Completed;
                    goal.updated_at = Utc::now();

                    let mut outbox_events = Vec::new();
                    if let (Some(task), Some(agent_id)) =
                        (terminal_task.as_ref(), agent_id.as_ref())
                    {
                        outbox_events.push(self.envelope_for(
                            SwarmEvent::TaskCompleted {
                                task_id: task.id,
                                agent_id: agent_id.clone(),
                            },
                            conversation_id,
                        ));
                    }
                    outbox_events.push(self.envelope_for(
                        SwarmEvent::IntentionCompleted {
                            intention_id: intention.id,
                            commitment_id: intention.commitment_id,
                            goal_id: intention.goal_id,
                        },
                        conversation_id,
                    ));
                    outbox_events.push(self.envelope_for(
                        SwarmEvent::CommitmentFulfilled {
                            commitment_id: commitment.id,
                            goal_id,
                        },
                        conversation_id,
                    ));
                    outbox_events.push(
                        self.envelope_for(SwarmEvent::GoalCompleted { goal_id }, conversation_id),
                    );

                    self.store
                        .commit_agent_state_transition(AgentStateTransition {
                            terminal_task: terminal_task.as_ref(),
                            intention: &intention,
                            replacement_intention: None,
                            replacement_tasks: &[],
                            revision: None,
                            commitment: Some(&commitment),
                            goal: Some(&goal),
                            outbox_events: &outbox_events,
                        })?;
                    self.flush_pending_events(out)?;
                    return Ok(goal_id);
                }
                PlanOutcome::Blocked {
                    terminal_task,
                    agent_id,
                    reason,
                } => {
                    let revision = IntentionRevision::new(
                        &intention,
                        None,
                        terminal_task.id,
                        TaskStatus::Blocked,
                        IntentionRevisionDecision::Suspend,
                        reason.clone(),
                    );

                    commitment.status = CommitmentStatus::Suspended;
                    commitment.updated_at = Utc::now();
                    goal.status = GoalStatus::Blocked;
                    goal.updated_at = Utc::now();

                    let outbox_events = vec![
                        self.envelope_for(
                            SwarmEvent::TaskBlocked {
                                task_id: terminal_task.id,
                                agent_id,
                                reason: reason.clone(),
                            },
                            conversation_id,
                        ),
                        self.envelope_for(
                            SwarmEvent::IntentionBlocked {
                                intention_id: intention.id,
                                commitment_id: intention.commitment_id,
                                goal_id: intention.goal_id,
                                reason: reason.clone(),
                            },
                            conversation_id,
                        ),
                        self.envelope_for(
                            SwarmEvent::IntentionRevised {
                                revision_id: revision.id,
                                superseded_intention_id: revision.superseded_intention_id,
                                replacement_intention_id: revision.replacement_intention_id,
                                decision: revision.decision,
                                reason: revision.reason.clone(),
                            },
                            conversation_id,
                        ),
                        self.envelope_for(
                            SwarmEvent::CommitmentSuspended {
                                commitment_id: commitment.id,
                                goal_id,
                                reason: reason.clone(),
                            },
                            conversation_id,
                        ),
                        self.envelope_for(
                            SwarmEvent::GoalBlocked { goal_id, reason },
                            conversation_id,
                        ),
                    ];

                    self.store
                        .commit_agent_state_transition(AgentStateTransition {
                            terminal_task: Some(&terminal_task),
                            intention: &intention,
                            replacement_intention: None,
                            replacement_tasks: &[],
                            revision: Some(&revision),
                            commitment: Some(&commitment),
                            goal: Some(&goal),
                            outbox_events: &outbox_events,
                        })?;
                    self.flush_pending_events(out)?;
                    return Ok(goal_id);
                }
                PlanOutcome::Failed {
                    terminal_task,
                    agent_id,
                    reason,
                } if replan_count < MAX_AUTOMATIC_REPLANS => {
                    replan_count += 1;
                    let replacement_tasks = self.engineering.plan_tasks(
                        goal_id,
                        plan_text,
                        self.config.director.default_budget_usd,
                    );
                    let replacement = self.new_intention(
                        goal_id,
                        commitment.id,
                        replan_count,
                        &replacement_tasks,
                    );
                    let revision = IntentionRevision::new(
                        &intention,
                        Some(replacement.id),
                        terminal_task.id,
                        TaskStatus::Failed,
                        IntentionRevisionDecision::Replan,
                        reason.clone(),
                    );

                    let mut outbox_events = vec![
                        self.envelope_for(
                            SwarmEvent::TaskFailed {
                                task_id: terminal_task.id,
                                agent_id,
                                reason: reason.clone(),
                            },
                            conversation_id,
                        ),
                        self.envelope_for(
                            SwarmEvent::IntentionFailed {
                                intention_id: intention.id,
                                commitment_id: intention.commitment_id,
                                goal_id: intention.goal_id,
                                reason: reason.clone(),
                            },
                            conversation_id,
                        ),
                        self.envelope_for(
                            SwarmEvent::IntentionRevised {
                                revision_id: revision.id,
                                superseded_intention_id: revision.superseded_intention_id,
                                replacement_intention_id: revision.replacement_intention_id,
                                decision: revision.decision,
                                reason: revision.reason.clone(),
                            },
                            conversation_id,
                        ),
                        self.envelope_for(
                            SwarmEvent::IntentionActivated {
                                intention_id: replacement.id,
                                commitment_id: replacement.commitment_id,
                                goal_id: replacement.goal_id,
                                owner_agent_id: replacement.owner_agent_id.clone(),
                            },
                            conversation_id,
                        ),
                    ];
                    for task in &replacement_tasks {
                        outbox_events.push(self.envelope_for(
                            SwarmEvent::TaskCreated {
                                task_id: task.id,
                                goal_id: task.goal_id,
                            },
                            conversation_id,
                        ));
                    }

                    self.store
                        .commit_agent_state_transition(AgentStateTransition {
                            terminal_task: Some(&terminal_task),
                            intention: &intention,
                            replacement_intention: Some(&replacement),
                            replacement_tasks: &replacement_tasks,
                            revision: Some(&revision),
                            commitment: None,
                            goal: None,
                            outbox_events: &outbox_events,
                        })?;
                    self.flush_pending_events(out)?;

                    intention = replacement;
                    tasks = replacement_tasks;
                    initialize_tasks = false;
                }
                PlanOutcome::Failed {
                    terminal_task,
                    agent_id,
                    reason,
                } => {
                    let revision = IntentionRevision::new(
                        &intention,
                        None,
                        terminal_task.id,
                        TaskStatus::Failed,
                        IntentionRevisionDecision::Abandon,
                        reason.clone(),
                    );

                    commitment.status = CommitmentStatus::Abandoned;
                    commitment.updated_at = Utc::now();
                    goal.status = GoalStatus::Failed;
                    goal.updated_at = Utc::now();

                    let outbox_events = vec![
                        self.envelope_for(
                            SwarmEvent::TaskFailed {
                                task_id: terminal_task.id,
                                agent_id,
                                reason: reason.clone(),
                            },
                            conversation_id,
                        ),
                        self.envelope_for(
                            SwarmEvent::IntentionFailed {
                                intention_id: intention.id,
                                commitment_id: intention.commitment_id,
                                goal_id: intention.goal_id,
                                reason: reason.clone(),
                            },
                            conversation_id,
                        ),
                        self.envelope_for(
                            SwarmEvent::IntentionRevised {
                                revision_id: revision.id,
                                superseded_intention_id: revision.superseded_intention_id,
                                replacement_intention_id: revision.replacement_intention_id,
                                decision: revision.decision,
                                reason: revision.reason.clone(),
                            },
                            conversation_id,
                        ),
                        self.envelope_for(
                            SwarmEvent::CommitmentAbandoned {
                                commitment_id: commitment.id,
                                goal_id,
                                reason: reason.clone(),
                            },
                            conversation_id,
                        ),
                        self.envelope_for(
                            SwarmEvent::GoalFailed { goal_id, reason },
                            conversation_id,
                        ),
                    ];

                    self.store
                        .commit_agent_state_transition(AgentStateTransition {
                            terminal_task: Some(&terminal_task),
                            intention: &intention,
                            replacement_intention: None,
                            replacement_tasks: &[],
                            revision: Some(&revision),
                            commitment: Some(&commitment),
                            goal: Some(&goal),
                            outbox_events: &outbox_events,
                        })?;
                    self.flush_pending_events(out)?;
                    return Ok(goal_id);
                }
            }
        }
    }

    fn new_intention(
        &self,
        goal_id: Uuid,
        commitment_id: Uuid,
        attempt: usize,
        tasks: &[Task],
    ) -> Intention {
        let description = if attempt == 0 {
            "Execute the selected engineering plan".to_string()
        } else {
            format!("Execute engineering replan attempt {attempt}")
        };
        let mut intention = Intention::new(
            goal_id,
            commitment_id,
            "manager-engineering",
            description,
            tasks.iter().map(|task| task.id).collect(),
        );
        intention.status = IntentionStatus::Active;
        intention.updated_at = Utc::now();
        intention
    }

    fn activate_intention(
        &self,
        intention: &Intention,
        out: &mut dyn Write,
    ) -> Result<(), RuntimeError> {
        self.store.upsert_intention(intention)?;
        self.emit_intention_activated(intention, out)
    }

    fn emit_intention_activated(
        &self,
        intention: &Intention,
        out: &mut dyn Write,
    ) -> Result<(), RuntimeError> {
        self.emit(
            out,
            SwarmEvent::IntentionActivated {
                intention_id: intention.id,
                commitment_id: intention.commitment_id,
                goal_id: intention.goal_id,
                owner_agent_id: intention.owner_agent_id.clone(),
            },
        )
    }

    fn execute_intention(
        &self,
        intention: &mut Intention,
        tasks: Vec<Task>,
        out: &mut dyn Write,
        initialize_tasks: bool,
        conversation_id: Option<Uuid>,
    ) -> Result<PlanOutcome, RuntimeError> {
        if initialize_tasks {
            for task in &tasks {
                self.store.upsert_task(task)?;
                self.emit_for(
                    out,
                    SwarmEvent::TaskCreated {
                        task_id: task.id,
                        goal_id: task.goal_id,
                    },
                    conversation_id,
                )?;
            }
        }

        let executable_tasks: Vec<Task> = tasks
            .into_iter()
            .filter(|task| task.status != TaskStatus::Completed)
            .collect();
        let task_count = executable_tasks.len();
        for (index, task) in executable_tasks.into_iter().enumerate() {
            let agent_id = task
                .assigned_agent_id
                .clone()
                .unwrap_or_else(|| self.worker.agent_id().to_string());
            let mut running = task;
            running.status = TaskStatus::Running;
            running.updated_at = Utc::now();
            self.store.upsert_task(&running)?;
            self.emit_for(
                out,
                SwarmEvent::TaskStarted {
                    task_id: running.id,
                    agent_id: agent_id.clone(),
                },
                conversation_id,
            )?;

            let report = self.worker.execute_with_report(&running);
            let reported_status = report.task.status;
            let worker_reason = report.reason;
            let (terminal_status, reason) = match reported_status {
                TaskStatus::Completed => (TaskStatus::Completed, None),
                TaskStatus::Blocked => (
                    TaskStatus::Blocked,
                    Some(
                        worker_reason
                            .unwrap_or_else(|| format!("task {} blocked by worker", running.id)),
                    ),
                ),
                TaskStatus::Failed => (
                    TaskStatus::Failed,
                    Some(worker_reason.unwrap_or_else(|| {
                        format!("task {} failed in worker execution", running.id)
                    })),
                ),
                other => (
                    TaskStatus::Failed,
                    Some(worker_reason.unwrap_or_else(|| {
                        format!(
                            "task {} returned non-terminal worker status {:?}",
                            running.id, other
                        )
                    })),
                ),
            };

            let mut result = running;
            result.status = terminal_status;
            result.updated_at = Utc::now();

            match terminal_status {
                TaskStatus::Completed if index + 1 < task_count => {
                    self.store.upsert_task(&result)?;
                    self.emit_for(
                        out,
                        SwarmEvent::TaskCompleted {
                            task_id: result.id,
                            agent_id,
                        },
                        conversation_id,
                    )?;
                }
                TaskStatus::Completed => {
                    intention.status = IntentionStatus::Completed;
                    intention.updated_at = Utc::now();
                    return Ok(PlanOutcome::Completed {
                        terminal_task: Some(result),
                        agent_id: Some(agent_id),
                    });
                }
                TaskStatus::Blocked => {
                    intention.status = IntentionStatus::Blocked;
                    intention.updated_at = Utc::now();
                    return Ok(PlanOutcome::Blocked {
                        terminal_task: result,
                        agent_id,
                        reason: reason.expect("blocked outcome has reason"),
                    });
                }
                TaskStatus::Failed => {
                    intention.status = IntentionStatus::Failed;
                    intention.updated_at = Utc::now();
                    return Ok(PlanOutcome::Failed {
                        terminal_task: result,
                        agent_id,
                        reason: reason.expect("failed outcome has reason"),
                    });
                }
                _ => unreachable!("worker result is normalized to a terminal status"),
            }
        }

        intention.status = IntentionStatus::Completed;
        intention.updated_at = Utc::now();
        Ok(PlanOutcome::Completed {
            terminal_task: None,
            agent_id: None,
        })
    }

    fn envelope_for(&self, event: SwarmEvent, conversation_id: Option<Uuid>) -> SwarmEventEnvelope {
        SwarmEventEnvelope::new(event, conversation_id)
    }

    pub fn flush_pending_events(&self, out: &mut dyn Write) -> Result<usize, RuntimeError> {
        const BATCH_SIZE: usize = 100;
        let mut delivered = 0usize;

        loop {
            let pending = self.store.pending_outbox(BATCH_SIZE)?;
            if pending.is_empty() {
                break;
            }
            let batch_len = pending.len();

            for record in pending {
                let event_id = record
                    .envelope
                    .event_id
                    .ok_or(StoreError::InvalidTransition(
                        "pending outbox event is missing a stable event id",
                    ))?;
                let line = format_event_line(&record.envelope)?;
                writeln!(out, "{}", line)?;
                out.flush()?;
                self.store.mark_outbox_delivered(event_id)?;
                delivered += 1;
            }

            if batch_len < BATCH_SIZE {
                break;
            }
        }

        Ok(delivered)
    }

    fn emit_for(
        &self,
        out: &mut dyn Write,
        event: SwarmEvent,
        conversation_id: Option<Uuid>,
    ) -> Result<(), RuntimeError> {
        let envelope = self.envelope_for(event, conversation_id);
        let line = format_event_line(&envelope)?;
        writeln!(out, "{}", line)?;
        out.flush()?;
        Ok(())
    }

    fn emit(&self, out: &mut dyn Write, event: SwarmEvent) -> Result<(), RuntimeError> {
        self.emit_for(out, event, Some(self.conversation_id))
    }
}

fn ordered_tasks_for_intention(
    tasks: &[Task],
    intention: &Intention,
) -> Result<Vec<Task>, RuntimeError> {
    let mut ordered = Vec::with_capacity(intention.planned_task_ids.len());
    for task_id in &intention.planned_task_ids {
        let task = tasks
            .iter()
            .find(|task| task.id == *task_id)
            .cloned()
            .ok_or(StoreError::InvalidTransition(
                "replacement intention references a missing task",
            ))?;
        ordered.push(task);
    }
    Ok(ordered)
}

pub fn init_project(dir: &Path) -> Result<(), RuntimeError> {
    AgentConfigFile::write_init(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ai_worker::TaskExecution;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct ScriptedWorker {
        statuses: Mutex<VecDeque<TaskStatus>>,
    }

    impl ScriptedWorker {
        fn new(statuses: impl IntoIterator<Item = TaskStatus>) -> Self {
            Self {
                statuses: Mutex::new(statuses.into_iter().collect()),
            }
        }
    }

    impl Worker for ScriptedWorker {
        fn agent_id(&self) -> &str {
            "worker-scripted"
        }

        fn execute(&self, task: &Task) -> Task {
            let mut task = task.clone();
            task.status = self
                .statuses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(TaskStatus::Completed);
            task.updated_at = Utc::now();
            task
        }

        fn execute_with_report(&self, task: &Task) -> TaskExecution {
            let task = self.execute(task);
            match task.status {
                TaskStatus::Blocked => {
                    TaskExecution::with_reason(task, "scripted external dependency is unavailable")
                }
                TaskStatus::Failed => {
                    TaskExecution::with_reason(task, "scripted worker execution failed")
                }
                _ => TaskExecution::new(task),
            }
        }
    }

    fn runtime_with_worker(tmp: &Path, statuses: Vec<TaskStatus>) -> LocalRuntime {
        init_project(tmp).unwrap();
        LocalRuntime::open_with_worker(tmp.to_path_buf(), Box::new(ScriptedWorker::new(statuses)))
            .unwrap()
    }

    struct FailOnTerminalFlushWriter {
        bytes: Vec<u8>,
        fail_next_flush: bool,
        failed_once: bool,
    }

    impl FailOnTerminalFlushWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                fail_next_flush: false,
                failed_once: false,
            }
        }
    }

    impl std::io::Write for FailOnTerminalFlushWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            if String::from_utf8_lossy(&self.bytes).contains("\"task_completed\"") {
                self.fail_next_flush = true;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_next_flush && !self.failed_once {
                self.failed_once = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected post-write flush failure",
                ));
            }
            Ok(())
        }
    }

    struct FailOnResumeFlushWriter {
        bytes: Vec<u8>,
        fail_next_flush: bool,
        failed_once: bool,
    }

    impl FailOnResumeFlushWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                fail_next_flush: false,
                failed_once: false,
            }
        }
    }

    impl std::io::Write for FailOnResumeFlushWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            if String::from_utf8_lossy(&self.bytes).contains("\"goal_resumed\"") {
                self.fail_next_flush = true;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_next_flush && !self.failed_once {
                self.failed_once = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected resume delivery failure",
                ));
            }
            Ok(())
        }
    }

    struct FailOnReplanFlushWriter {
        bytes: Vec<u8>,
        fail_next_flush: bool,
        failed_once: bool,
    }

    impl FailOnReplanFlushWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                fail_next_flush: false,
                failed_once: false,
            }
        }
    }

    impl std::io::Write for FailOnReplanFlushWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            if String::from_utf8_lossy(&self.bytes).contains("\"intention_revised\"") {
                self.fail_next_flush = true;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_next_flush && !self.failed_once {
                self.failed_once = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected replan delivery failure",
                ));
            }
            Ok(())
        }
    }

    #[test]
    fn runtime_emits_nlap_events() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();
        let mut rt = LocalRuntime::open(tmp.clone()).unwrap();
        let mut buf = std::io::Cursor::new(Vec::new());
        let goal_id = rt.handle_user_message("ship feature X", &mut buf).unwrap();
        assert!(!goal_id.is_nil());
        let text = String::from_utf8(buf.into_inner()).unwrap();
        assert!(text.contains("goal_created"));
        assert!(text.contains("commitment_activated"));
        assert!(text.contains("intention_activated"));
        assert!(text.contains("task_created"));
        assert!(text.contains("intention_completed"));
        assert!(text.contains("commitment_fulfilled"));
        let graph = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Completed);
        assert_eq!(graph.commitments.len(), 1);
        assert_eq!(graph.commitments[0].status, CommitmentStatus::Fulfilled);
        assert_eq!(graph.intentions.len(), 1);
        assert_eq!(graph.intentions[0].status, IntentionStatus::Completed);
        assert!(graph.intention_revisions.is_empty());
        assert_eq!(
            graph.intentions[0].planned_task_ids.len(),
            graph.tasks.len()
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn failed_task_replans_once_and_can_recover() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-replan-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut rt = runtime_with_worker(&tmp, vec![TaskStatus::Failed, TaskStatus::Completed]);
        let mut buf = std::io::Cursor::new(Vec::new());
        let goal_id = rt.handle_user_message("ship feature X", &mut buf).unwrap();
        let text = String::from_utf8(buf.into_inner()).unwrap();
        assert!(text.contains("task_failed"));
        assert!(text.contains("intention_revised"));
        assert!(text.contains("commitment_fulfilled"));

        let graph = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Completed);
        assert_eq!(graph.intentions.len(), 2);
        assert_eq!(graph.intentions[0].status, IntentionStatus::Failed);
        assert_eq!(graph.intentions[1].status, IntentionStatus::Completed);
        assert_eq!(graph.intention_revisions.len(), 1);
        assert_eq!(
            graph.intention_revisions[0].decision,
            IntentionRevisionDecision::Replan
        );
        assert_eq!(
            graph.intention_revisions[0].reason,
            "scripted worker execution failed"
        );
        assert_eq!(
            graph.intention_revisions[0].replacement_intention_id,
            Some(graph.intentions[1].id)
        );
        assert_eq!(graph.commitments[0].status, CommitmentStatus::Fulfilled);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn blocked_task_suspends_commitment_and_goal() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-blocked-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut rt = runtime_with_worker(&tmp, vec![TaskStatus::Blocked]);
        let mut buf = std::io::Cursor::new(Vec::new());
        let goal_id = rt.handle_user_message("ship feature X", &mut buf).unwrap();
        let text = String::from_utf8(buf.into_inner()).unwrap();
        assert!(text.contains("task_blocked"));
        assert!(text.contains("commitment_suspended"));
        assert!(text.contains("goal_blocked"));

        let graph = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Blocked);
        assert_eq!(graph.intentions[0].status, IntentionStatus::Blocked);
        assert_eq!(graph.commitments[0].status, CommitmentStatus::Suspended);
        assert_eq!(graph.intention_revisions.len(), 1);
        assert_eq!(
            graph.intention_revisions[0].decision,
            IntentionRevisionDecision::Suspend
        );
        assert_eq!(
            graph.intention_revisions[0].reason,
            "scripted external dependency is unavailable"
        );
        assert!(graph.intention_revisions[0]
            .replacement_intention_id
            .is_none());

        let mut conflicting = graph.intention_revisions[0].clone();
        conflicting.reason = "attempted revision rewrite".into();
        rt.store().insert_intention_revision(&conflicting).unwrap();
        let graph_after_retry = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(
            graph_after_retry.intention_revisions[0].reason,
            "scripted external dependency is unavailable"
        );

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn committed_outbox_event_retries_with_same_id_after_delivery_ack_failure() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-outbox-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();
        let mut rt = LocalRuntime::open(tmp.clone()).unwrap();

        // Fail the flush immediately after the first durable terminal event was
        // written, but before its outbox acknowledgement can be persisted.
        let mut failing = FailOnTerminalFlushWriter::new();
        let err = rt
            .handle_user_message("ship feature X", &mut failing)
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Io(_)));

        let goal = rt.store().list_goals().unwrap().remove(0);
        assert_eq!(goal.status, GoalStatus::Completed);

        let pending = rt.store().pending_outbox(10).unwrap();
        assert_eq!(pending.len(), 4);
        let first_event_id = pending[0].envelope.event_id.unwrap();
        assert!(
            String::from_utf8_lossy(&failing.bytes).contains(&first_event_id.to_string()),
            "the first outbox event reached the stream before acknowledgement failed"
        );

        let mut recovered = std::io::Cursor::new(Vec::new());
        assert_eq!(rt.flush_pending_events(&mut recovered).unwrap(), 4);
        assert!(rt.store().pending_outbox(10).unwrap().is_empty());

        let recovered_text = String::from_utf8(recovered.into_inner()).unwrap();
        let first_line = recovered_text.lines().next().unwrap();
        let retried: SwarmEventEnvelope = serde_json::from_str(first_line).unwrap();
        assert_eq!(retried.event_id, Some(first_event_id));
        assert!(matches!(retried.event, SwarmEvent::TaskCompleted { .. }));

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn resume_goal_survives_restart_after_commit_before_execution() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-resume-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();

        let mut first = runtime_with_worker(&tmp, vec![TaskStatus::Blocked]);
        let mut initial_out = std::io::Cursor::new(Vec::new());
        let goal_id = first
            .handle_user_message("ship feature X", &mut initial_out)
            .unwrap();
        let blocked_graph = first.store().get_goal_graph(goal_id).unwrap();
        let original_conversation_id = blocked_graph.goal.conversation_id;
        assert_eq!(blocked_graph.goal.status, GoalStatus::Blocked);
        drop(first);

        let request_id = Uuid::new_v4();
        let mut second = LocalRuntime::open_with_worker(
            tmp.clone(),
            Box::new(ScriptedWorker::new([TaskStatus::Completed])),
        )
        .unwrap();
        assert_ne!(Some(second.conversation_id()), original_conversation_id);

        let mut failing = FailOnResumeFlushWriter::new();
        let err = second
            .resume_goal(
                goal_id,
                request_id,
                "external dependency recovered",
                &mut failing,
            )
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Io(_)));

        let committed = second.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(committed.goal.status, GoalStatus::Running);
        assert_eq!(committed.resumptions.len(), 1);
        assert_eq!(committed.resumptions[0].request_id, request_id);
        assert_eq!(committed.intentions.len(), 2);
        assert_eq!(committed.intentions[1].status, IntentionStatus::Active);
        assert_eq!(
            ordered_tasks_for_intention(&committed.tasks, &committed.intentions[1]).unwrap()[0]
                .status,
            TaskStatus::Created
        );
        drop(second);

        let mut third = LocalRuntime::open_with_worker(
            tmp.clone(),
            Box::new(ScriptedWorker::new([TaskStatus::Completed])),
        )
        .unwrap();
        let mut recovered_out = std::io::Cursor::new(Vec::new());
        assert_eq!(
            third
                .resume_goal(
                    goal_id,
                    request_id,
                    "external dependency recovered",
                    &mut recovered_out,
                )
                .unwrap(),
            goal_id
        );

        let completed = third.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(completed.goal.status, GoalStatus::Completed);
        assert_eq!(completed.resumptions.len(), 1);
        assert_eq!(completed.intentions.len(), 2);
        assert_eq!(completed.intentions[1].status, IntentionStatus::Completed);
        assert_eq!(completed.commitments[0].status, CommitmentStatus::Fulfilled);

        let first_attempt_text = String::from_utf8_lossy(&failing.bytes);
        assert!(first_attempt_text.contains("commitment_resumed"));
        assert!(first_attempt_text.contains("goal_resumed"));

        let recovered_text = String::from_utf8(recovered_out.into_inner()).unwrap();
        assert!(recovered_text.contains("goal_resumed"));
        assert!(!recovered_text.contains("commitment_resumed"));
        assert!(recovered_text.contains("task_completed"));
        for line in first_attempt_text.lines().chain(recovered_text.lines()) {
            let envelope: SwarmEventEnvelope = serde_json::from_str(line).unwrap();
            assert_eq!(envelope.conversation_id, original_conversation_id);
        }

        let intentions_before = completed.intentions.len();
        let tasks_before = completed.tasks.len();
        drop(third);

        let mut fourth = LocalRuntime::open_with_worker(
            tmp.clone(),
            Box::new(ScriptedWorker::new([TaskStatus::Completed])),
        )
        .unwrap();
        let mut duplicate_out = std::io::Cursor::new(Vec::new());
        assert_eq!(
            fourth
                .resume_goal(
                    goal_id,
                    request_id,
                    "external dependency recovered",
                    &mut duplicate_out,
                )
                .unwrap(),
            goal_id
        );
        assert!(duplicate_out.into_inner().is_empty());

        let duplicate = fourth.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(duplicate.resumptions.len(), 1);
        assert_eq!(duplicate.intentions.len(), intentions_before);
        assert_eq!(duplicate.tasks.len(), tasks_before);

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn resumed_goal_recovers_latest_active_replan_after_restart() {
        let tmp =
            std::env::temp_dir().join(format!("nulang-agent-resume-replan-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();

        let mut first = runtime_with_worker(&tmp, vec![TaskStatus::Blocked]);
        let mut initial_out = std::io::Cursor::new(Vec::new());
        let goal_id = first
            .handle_user_message("ship feature X", &mut initial_out)
            .unwrap();
        drop(first);

        let request_id = Uuid::new_v4();
        let mut second = LocalRuntime::open_with_worker(
            tmp.clone(),
            Box::new(ScriptedWorker::new([
                TaskStatus::Failed,
                TaskStatus::Completed,
            ])),
        )
        .unwrap();
        let mut failing = FailOnReplanFlushWriter::new();
        let err = second
            .resume_goal(goal_id, request_id, "dependency recovered", &mut failing)
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Io(_)));

        let replanned = second.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(replanned.goal.status, GoalStatus::Running);
        assert_eq!(replanned.resumptions.len(), 1);
        assert_eq!(replanned.intentions.len(), 3);
        assert_eq!(replanned.intentions[0].status, IntentionStatus::Blocked);
        assert_eq!(replanned.intentions[1].status, IntentionStatus::Failed);
        assert_eq!(replanned.intentions[2].status, IntentionStatus::Active);
        let replan_tasks =
            ordered_tasks_for_intention(&replanned.tasks, &replanned.intentions[2]).unwrap();
        assert_eq!(replan_tasks.len(), 1);
        assert_eq!(replan_tasks[0].status, TaskStatus::Created);
        drop(second);

        let mut third = LocalRuntime::open_with_worker(
            tmp.clone(),
            Box::new(ScriptedWorker::new([TaskStatus::Completed])),
        )
        .unwrap();
        let mut recovered_out = std::io::Cursor::new(Vec::new());
        third
            .resume_goal(goal_id, request_id, "dependency recovered", &mut recovered_out)
            .unwrap();

        let completed = third.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(completed.goal.status, GoalStatus::Completed);
        assert_eq!(completed.resumptions.len(), 1);
        assert_eq!(completed.intentions.len(), 3);
        assert_eq!(completed.intentions[2].status, IntentionStatus::Completed);
        assert!(String::from_utf8(recovered_out.into_inner())
            .unwrap()
            .contains("task_completed"));

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn repeated_failure_abandons_commitment_after_bounded_replan() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-failed-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut rt = runtime_with_worker(&tmp, vec![TaskStatus::Failed, TaskStatus::Failed]);
        let mut buf = std::io::Cursor::new(Vec::new());
        let goal_id = rt.handle_user_message("ship feature X", &mut buf).unwrap();
        let text = String::from_utf8(buf.into_inner()).unwrap();
        assert!(text.contains("commitment_abandoned"));
        assert!(text.contains("goal_failed"));

        let graph = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Failed);
        assert_eq!(graph.intentions.len(), 2);
        assert_eq!(graph.intentions[0].status, IntentionStatus::Failed);
        assert_eq!(graph.intentions[1].status, IntentionStatus::Failed);
        assert_eq!(graph.intention_revisions.len(), 2);
        assert_eq!(
            graph.intention_revisions[0].decision,
            IntentionRevisionDecision::Replan
        );
        assert_eq!(
            graph.intention_revisions[1].decision,
            IntentionRevisionDecision::Abandon
        );
        assert_eq!(graph.commitments[0].status, CommitmentStatus::Abandoned);
        let _ = std::fs::remove_dir_all(tmp);
    }
}
