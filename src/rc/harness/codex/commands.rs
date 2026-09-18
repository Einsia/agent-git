use super::*;
use anyhow::{Context, ensure};

impl CodexDriver {
    pub async fn runtime_command(&mut self, name: &str, arguments: Value) -> crate::Result<Value> {
        ensure!(
            self.thread_id.is_some() && self.handshake_request.is_none(),
            "The Codex thread is still opening; no command was sent"
        );
        if name == "commands" {
            let mut catalog = json!({"commands": [
                {"name":"personality","description":"Choose how Codex responds","argument_hint":"[none | friendly | pragmatic]"},
                {"name":"fast","description":"Choose Fast service when the current model supports it","argument_hint":"[on | off]"},
                {"name":"diff","description":"Show staged and unstaged code changes"},
                {"name":"goal","description":"Set, edit, pause, resume, or clear a task goal","argument_hint":"[objective | edit | pause | resume | clear]"},
                {"name":"compact","description":"Summarize the conversation to free context"},
                {"name":"review","description":"Review pending changes, a branch, or a commit","argument_hint":"[base <branch> | commit <sha> | instructions]"},
                {"name":"plan","description":"Plan the work before making changes","argument_hint":"[prompt]"},
                {"name":"init","description":"Create project instructions in AGENTS.md"},
                {"name":"approve","description":"Approve one retry of a recent auto-review denial"},
                {"name":"feedback","description":"Send feedback to OpenAI with optional logs"},
                {"name":"status","description":"Show this conversation and its runtime status"},
                {"name":"memories","description":"Choose native memory mode for this conversation","argument_hint":"[enabled | disabled]"},
                {"name":"mcp","description":"Show connected MCP servers and tools"},
                {"name":"skills","description":"Show skills available in this project"},
                {"name":"apps","description":"Show available connected apps"}
            ]});
            match self
                .command_request(
                    "skills/list",
                    json!({"cwds":[self.cwd],"forceReload":false}),
                )
                .await
            {
                Ok(skills) => {
                    for skill in enabled_skills(&skills) {
                        if !catalog["commands"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|command| command["name"] == skill["name"])
                        {
                            catalog["commands"].as_array_mut().unwrap().push(json!({"name":skill["name"],"description":skill["description"],"kind":"skill"}));
                        }
                    }
                }
                Err(_) => catalog["skillsUnavailable"] = json!(true),
            }
            if let Ok(prompts) = super::prompts::catalog() {
                catalog["commands"].as_array_mut().unwrap().extend(prompts);
            }
            return Ok(catalog);
        }
        if name == "prompt.expand" {
            let name = arguments["name"]
                .as_str()
                .context("Custom prompt name is required")?;
            return Ok(
                json!({"prompt":super::prompts::expand(name, arguments["value"].as_str().unwrap_or_default())?}),
            );
        }
        if name == "skill.expand" {
            let skills = self
                .command_request("skills/list", json!({"cwds":[self.cwd],"forceReload":true}))
                .await?;
            let name = arguments["name"]
                .as_str()
                .context("Skill name is required")?;
            ensure!(
                enabled_skills(&skills).any(|skill| skill["name"] == name),
                "This skill is not enabled in this project"
            );
            return Ok(
                json!({"prompt":format!("${name} {}",arguments["value"].as_str().unwrap_or_default())}),
            );
        }
        if matches!(name, "personality" | "fast") {
            let models = self
                .command_request("model/list", json!({"includeHidden":false}))
                .await?;
            let model = models["data"]
                .as_array()
                .and_then(|rows| {
                    rows.iter().find(|model| {
                        self.model
                            .as_deref()
                            .is_some_and(|id| model["id"] == id || model["model"] == id)
                            || (self.model.is_none() && model["isDefault"] == true)
                    })
                })
                .context("The current model is absent from the native model catalog")?;
            let value = arguments["value"].as_str().unwrap_or_default();
            if name == "personality" {
                ensure!(
                    model["supportsPersonality"] == true,
                    "The current model does not support personalities"
                );
                if value.is_empty() {
                    return Ok(
                        json!({"choices":[{"id":"none","name":"Default"},{"id":"friendly","name":"Friendly"},{"id":"pragmatic","name":"Pragmatic"}]}),
                    );
                }
                ensure!(
                    matches!(value, "none" | "friendly" | "pragmatic"),
                    "Choose none, friendly, or pragmatic"
                );
                self.turn_options.insert("personality".into(), json!(value));
            } else {
                ensure!(
                    matches!(value, "" | "on" | "off"),
                    "Use /fast on or /fast off"
                );
                let fast = model["serviceTiers"].as_array().and_then(|tiers| {
                    tiers
                        .iter()
                        .find(|tier| tier["id"] == "fast" || tier["name"] == "Fast")
                });
                let fast = fast.context("Fast service is not offered for the current model")?;
                if value.is_empty() {
                    return Ok(
                        json!({"choices":[{"id":"on","name":"Fast on","description":fast["description"]},{"id":"off","name":"Fast off"}]}),
                    );
                }
                self.turn_options.insert(
                    "serviceTier".into(),
                    if value == "on" {
                        fast["id"].clone()
                    } else {
                        model["defaultServiceTier"].clone()
                    },
                );
            }
            return Ok(json!({"text":format!("{}: {}. Applies to the next message.",name,value)}));
        }
        if name == "diff" {
            let mut command = crate::infra::git_runtime::command();
            command
                .args(["diff", "HEAD", "--no-ext-diff", "--no-textconv", "--"])
                .current_dir(&self.cwd);
            return capture_diff(
                command,
                std::time::Instant::now() + std::time::Duration::from_secs(10),
            )
            .await;
        }
        let thread = self
            .thread_id
            .clone()
            .context("The Codex thread is still opening")?;
        if name == "feedback" {
            let params = feedback_params(&thread, &arguments)?;
            let result = self.command_request("feedback/upload", params).await?;
            return Ok(json!({"text":"Feedback sent to OpenAI.","feedbackId":result["threadId"]}));
        }
        if name == "approve" {
            let id = arguments["value"].as_str().unwrap_or_default();
            if id.is_empty() {
                return Ok(self.guardian_denials.choices());
            }
            let event = self
                .guardian_denials
                .take(id)
                .context("That auto-review denial is no longer available")?;
            self.command_request(
                "thread/approveGuardianDeniedAction",
                json!({"threadId":thread,"event":event}),
            )
            .await?;
            return Ok(
                json!({"text":"Approval recorded for one retry. Native auto-review still applies."}),
            );
        }
        if name == "memories" {
            let value = arguments["value"].as_str().unwrap_or_default();
            if value.is_empty() {
                return Ok(
                    json!({"choices":[{"id":"enabled","name":"Enable memories","description":"Enable the runtime's memory mode for this conversation"},{"id":"disabled","name":"Disable memories","description":"Disable the runtime's memory mode for this conversation"}]}),
                );
            }
            ensure!(
                matches!(value, "enabled" | "disabled"),
                "Choose enabled or disabled for memories"
            );
            self.command_request(
                "thread/memoryMode/set",
                json!({"threadId":thread,"mode":value}),
            )
            .await?;
            return Ok(json!({"text":format!("Memory mode is {value} for this conversation.")}));
        }
        if name == "status" {
            let mut result = self
                .command_request(
                    "thread/read",
                    json!({"threadId":thread,"includeTurns":false}),
                )
                .await?;
            result["tokenUsage"] = self.token_usage.clone().unwrap_or(Value::Null);
            match self
                .command_request("account/rateLimits/read", json!({}))
                .await
            {
                Ok(limits) => result["limits"] = limits,
                Err(_) => result["limitsUnavailable"] = json!(true),
            }
            return Ok(result);
        }
        ensure!(arguments.is_object(), "Command arguments must be an object");
        let (method, params) = match name {
            "goal.get" => ("thread/goal/get", json!({"threadId":thread})),
            "goal.clear" => ("thread/goal/clear", json!({"threadId":thread})),
            "goal.set" => {
                if arguments["status"] == "active"
                    || (arguments["status"] != "paused" && arguments.get("objective").is_some())
                {
                    ensure!(
                        self.pending_mode.is_none(),
                        "Send a message to apply the pending permission mode before starting a goal"
                    );
                }
                let mut params = json!({"threadId":thread});
                if let Some(objective) = arguments.get("objective") {
                    let objective = objective
                        .as_str()
                        .context("Goal objective must be text")?
                        .trim();
                    ensure!(
                        !objective.is_empty() && objective.chars().count() <= 4000,
                        "Goal objective must contain between 1 and 4000 characters"
                    );
                    params["objective"] = json!(objective);
                }
                if let Some(status) = arguments.get("status") {
                    ensure!(
                        matches!(status.as_str(), Some("active" | "paused")),
                        "Use active or paused to control a goal"
                    );
                    params["status"] = status.clone();
                }
                if let Some(budget) = arguments.get("tokenBudget") {
                    ensure!(
                        budget.is_null() || budget.as_i64().is_some_and(|n| n > 0),
                        "Token budget must be a positive integer or null"
                    );
                    params["tokenBudget"] = budget.clone();
                }
                ("thread/goal/set", params)
            }
            "compact" | "review" => {
                ensure!(
                    self.current_turn.is_none() && self.pending_turn_start.is_none(),
                    "Wait for the current turn before starting this command"
                );
                ensure!(
                    self.pending_mode.is_none(),
                    "Send a message to apply the pending permission mode before starting this command"
                );
                if name == "compact" {
                    ("thread/compact/start", json!({"threadId":thread}))
                } else {
                    let target = arguments
                        .get("target")
                        .cloned()
                        .unwrap_or_else(|| json!({"type":"uncommittedChanges"}));
                    ensure!(
                        matches!(
                            target["type"].as_str(),
                            Some("uncommittedChanges" | "baseBranch" | "commit" | "custom")
                        ),
                        "Choose a valid code review target"
                    );
                    (
                        "review/start",
                        json!({"threadId":thread,"delivery":"inline","target":target}),
                    )
                }
            }
            "mcp" => ("mcpServerStatus/list", json!({"limit":100})),
            "skills" => ("skills/list", json!({"cwds":[self.cwd],"forceReload":true})),
            "apps" => ("app/list", json!({"threadId":thread,"limit":100})),
            _ => anyhow::bail!("This command is not available in Codex"),
        };
        self.command_request(method, params).await
    }

    pub(super) async fn command_request(
        &mut self,
        method: &str,
        params: Value,
    ) -> crate::Result<Value> {
        ensure!(
            self.command_requests.len() < 128,
            "Too many Codex commands are awaiting responses"
        );
        let id = self.alloc_id()?;
        self.command_requests.insert(id);
        self.send(&json!({"id":id,"method":method,"params":params}))
            .await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let queued = tokio::time::timeout_at(deadline, self.proc.next())
                .await
                .context("Codex command response timed out; refresh its state before retrying")?
                .context("Codex exited before answering the command")?;
            if let Line::Json(value) = queued.line()
                && value["method"] == "thread/tokenUsage/updated"
                && value["params"]["threadId"].as_str() == self.thread_id.as_deref()
            {
                self.token_usage = Some(value["params"]["tokenUsage"].clone());
            }
            if let Line::Json(value) = queued.line()
                && value.get("method").is_none()
                && value["id"].as_i64() == Some(id)
            {
                self.command_requests.remove(&id);
                if let Some(error) = value.get("error") {
                    anyhow::bail!(
                        "{}",
                        error["message"]
                            .as_str()
                            .unwrap_or("Codex refused the command")
                    );
                }
                return value
                    .get("result")
                    .cloned()
                    .context("Codex returned no command result");
            }
            let eof = matches!(queued.line(), Line::Eof);
            self.pushback.push(queued);
            ensure!(!eof, "Codex exited before answering the command");
        }
    }
}

async fn capture_diff(
    command: std::process::Command,
    deadline: std::time::Instant,
) -> crate::Result<Value> {
    let output = tokio::task::spawn_blocking(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(crate::infra::local_git::output_until(
                command,
                512 * 1024,
                deadline,
            ))
    })
    .await?
    .context("The diff could not be read within display limits; use the repository review panel")?;
    ensure!(
        output.status.success(),
        "Git could not read the working tree diff"
    );
    Ok(json!({"text":format!("```diff\n{}\n```", String::from_utf8_lossy(&output.stdout))}))
}

fn feedback_params(thread: &str, arguments: &Value) -> crate::Result<Value> {
    ensure!(
        arguments["confirmed"] == true,
        "Confirm feedback submission in the feedback dialog"
    );
    let classification = arguments["classification"]
        .as_str()
        .context("Choose a feedback category")?;
    ensure!(
        matches!(
            classification,
            "bug" | "bad_result" | "good_result" | "safety_check" | "other"
        ),
        "Choose a supported feedback category"
    );
    let reason = arguments["reason"]
        .as_str()
        .context("Feedback must be text")?
        .trim();
    ensure!(
        reason.chars().count() <= 4000,
        "Feedback cannot exceed 4000 characters"
    );
    let include_logs = arguments["includeLogs"]
        .as_bool()
        .context("Choose whether to include logs")?;
    Ok(
        json!({"threadId":thread,"classification":classification,"reason":if reason.is_empty(){None}else{Some(reason)},"includeLogs":include_logs}),
    )
}

fn enabled_skills(value: &Value) -> impl Iterator<Item = &Value> {
    value["data"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|entry| entry["skills"].as_array().into_iter().flatten())
        .filter(|skill| {
            skill["enabled"] == true
                && skill["name"].as_str().is_some_and(|name| {
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_alphanumeric() || "-_:".contains(c))
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn diff_capture_bounds_output_and_reaps_timed_out_children() {
        use std::time::{Duration, Instant};
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf 'small diff'"]);
        let result = capture_diff(command, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result["text"].as_str().unwrap().contains("small diff"));
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "yes oversized"]);
        let error = capture_diff(command, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("exceeds its limit"));
        let directory = tempfile::tempdir().unwrap();
        let pid = directory.path().join("child.pid");
        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", "sleep 30 & echo $! > \"$1\"; wait", "diff-fixture"])
            .arg(&pid);
        let error = capture_diff(command, Instant::now() + Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("time limit"));
        let child: i32 = std::fs::read_to_string(pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(child, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[tokio::test]
    async fn commands_wait_for_the_native_handshake_before_allocating_or_writing_a_request() {
        let mut driver = CodexDriver::test_responder(None, &[]);
        for name in ["commands", "goal.set", "feedback", "approve"] {
            let error = driver.runtime_command(name, json!({})).await.unwrap_err();
            assert_eq!(
                error.to_string(),
                "The Codex thread is still opening; no command was sent"
            );
        }
        driver.set_test_thread_id("thread");
        driver.handshake_request = Some((0, "thread/start"));
        assert!(driver.runtime_command("commands", json!({})).await.is_err());
        assert_eq!(driver.next_id, Some(1));
        assert!(driver.command_requests.is_empty());
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn feedback_requires_confirmation_and_cannot_upload_caller_selected_files_or_threads() {
        let mut args = json!({"classification":"bug","reason":" Fixture ","includeLogs":false,"threadId":"foreign","extraLogFiles":["/private/file"],"tags":{"untrusted":"value"}});
        let mut driver = CodexDriver::test_responder(
            Some("thread"),
            &[json!({"id":1,"result":{"threadId":"receipt"}})],
        );
        assert!(
            driver
                .runtime_command("feedback", args.clone())
                .await
                .is_err()
        );
        assert!(driver.command_requests.is_empty());
        args["confirmed"] = json!(true);
        assert_eq!(
            feedback_params("thread", &args).unwrap(),
            json!({"threadId":"thread","classification":"bug","reason":"Fixture","includeLogs":false})
        );
        let result = driver
            .runtime_command("feedback", args.clone())
            .await
            .unwrap();
        assert_eq!(result["feedbackId"], "receipt");
        assert!(driver.pending_turn_start.is_none());
        args["classification"] = json!("unknown");
        assert!(feedback_params("thread", &args).is_err());
        args["classification"] = json!("other");
        args["reason"] = json!("x".repeat(4001));
        assert!(feedback_params("thread", &args).is_err());
        args["reason"] = json!("");
        args["includeLogs"] = Value::Null;
        assert!(feedback_params("thread", &args).is_err());
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn approval_retry_uses_only_a_native_denial_in_the_current_thread_once() {
        let event = json!({"threadId":"foreign","reviewId":"review","turnId":"turn","startedAtMs":1,"completedAtMs":2,"decisionSource":"agent","review":{"status":"denied","rationale":"Fixture refusal"},"action":{"type":"command","source":"shell","command":"printf fixture","cwd":"/tmp"}});
        let mut driver =
            CodexDriver::test_responder(Some("thread"), &[json!({"id":1,"result":{}})]);
        driver
            .classify(json!({"method":"item/autoApprovalReview/completed","params":event}))
            .await;
        assert!(
            driver
                .runtime_command("approve", json!({"value":"review","event":event}))
                .await
                .is_err()
        );
        let mut event = event;
        event["threadId"] = json!("thread");
        driver
            .classify(json!({"method":"item/autoApprovalReview/completed","params":event}))
            .await;
        let choices = driver.runtime_command("approve", json!({})).await.unwrap();
        assert_eq!(choices["choices"][0]["name"], "printf fixture");
        driver
            .runtime_command(
                "approve",
                json!({"value":"review","event":{"action":"forged"}}),
            )
            .await
            .unwrap();
        assert!(driver.pending_turn_start.is_none());
        driver
            .classify(json!({"method":"item/autoApprovalReview/completed","params":event}))
            .await;
        assert!(
            driver
                .runtime_command("approve", json!({"value":"review"}))
                .await
                .is_err()
        );
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn memory_mode_uses_native_control_without_submitting_a_turn() {
        let mut driver =
            CodexDriver::test_responder(Some("thread"), &[json!({"id":1,"result":{}})]);
        let choices = driver.runtime_command("memories", json!({})).await.unwrap();
        assert_eq!(choices["choices"][0]["id"], "enabled");
        assert!(
            driver
                .runtime_command("memories", json!({"value":"reset"}))
                .await
                .is_err()
        );
        let result = driver
            .runtime_command("memories", json!({"value":"disabled"}))
            .await
            .unwrap();
        assert_eq!(
            result["text"],
            "Memory mode is disabled for this conversation."
        );
        assert!(driver.pending_turn_start.is_none());
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn skill_commands_use_only_enabled_native_project_entries() {
        let skills = json!({"data":[{"skills":[{"name":"verify","description":"Check project behavior","enabled":true},{"name":"hidden","enabled":false}]}]});
        let mut driver = CodexDriver::test_responder(
            Some("thread"),
            &[
                json!({"id":1,"result":skills}),
                json!({"id":2,"result":skills}),
                json!({"id":3,"result":skills}),
            ],
        );
        let catalog = driver.runtime_command("commands", json!({})).await.unwrap();
        assert!(
            catalog["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command["name"] == "verify" && command["kind"] == "skill")
        );
        assert!(
            !catalog["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command["name"] == "hidden")
        );
        assert_eq!(
            driver
                .runtime_command("skill.expand", json!({"name":"verify","value":"the UI"}))
                .await
                .unwrap()["prompt"],
            "$verify the UI"
        );
        assert!(
            driver
                .runtime_command("skill.expand", json!({"name":"hidden"}))
                .await
                .is_err()
        );
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn status_includes_interleaved_native_usage_and_preserves_account_refusals() {
        let usage = json!({"last":{"totalTokens":50},"total":{"totalTokens":100},"modelContextWindow":1000});
        let mut driver = CodexDriver::test_responder(
            Some("thread"),
            &[
                json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread","tokenUsage":usage}}),
                json!({"id":1,"result":{"thread":{"id":"thread"}}}),
                json!({"id":2,"error":{"message":"No account rate limits"}}),
            ],
        );
        let result = driver.runtime_command("status", json!({})).await.unwrap();
        assert_eq!(result["tokenUsage"], usage);
        assert_eq!(result["limitsUnavailable"], true);
        assert_eq!(result["thread"]["id"], "thread");
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn goal_control_preserves_interleaved_native_updates_and_rejects_other_methods() {
        let goal = json!({"threadId":"thread","objective":"Finish the fixture","status":"paused","tokensUsed":42});
        let mut driver = CodexDriver::test_responder(
            Some("thread"),
            &[
                json!({"method":"thread/goal/updated","params":{"threadId":"thread","goal":goal}}),
                json!({"id":1,"result":{"goal":goal}}),
            ],
        );
        assert!(
            driver
                .runtime_command("thread/fork", json!({}))
                .await
                .is_err()
        );
        assert!(
            driver
                .runtime_command("goal.set", json!({"objective":" "}))
                .await
                .is_err()
        );
        let result = driver
            .runtime_command(
                "goal.set",
                json!({"objective":"Finish the fixture","status":"paused"}),
            )
            .await
            .unwrap();
        assert_eq!(result["goal"], goal);
        assert!(
            matches!(driver.next_event().await,Some(HarnessEvent::GoalUpdated{goal:updated}) if updated==goal)
        );
        assert!(driver.command_requests.is_empty());
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn autonomy_cannot_start_under_a_permission_mode_waiting_to_be_applied() {
        let mut driver = CodexDriver::test_responder(Some("thread"), &[]);
        driver.pending_mode = Some(PermissionMode::Plan);
        for (name, args) in [
            ("goal.set", json!({"objective":"Finish","status":"active"})),
            ("review", json!({})),
            ("compact", json!({})),
        ] {
            assert!(
                driver
                    .runtime_command(name, args)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("pending permission mode")
            );
        }
        assert!(driver.command_requests.is_empty());
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn editing_a_paused_goal_preserves_the_pending_permission_change() {
        let goal = json!({"objective":"Revised objective","status":"paused"});
        let mut driver =
            CodexDriver::test_responder(Some("thread"), &[json!({"id":1,"result":{"goal":goal}})]);
        driver.pending_mode = Some(PermissionMode::Plan);
        let result = driver
            .runtime_command("goal.set", goal.clone())
            .await
            .unwrap();
        assert_eq!(result["goal"], goal);
        assert_eq!(driver.pending_mode, Some(PermissionMode::Plan));
        assert!(driver.pending_turn_start.is_none());
        driver.shutdown().await.unwrap();
    }
}
