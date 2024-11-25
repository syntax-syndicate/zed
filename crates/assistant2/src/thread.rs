use std::sync::Arc;

use futures::StreamExt as _;
use gpui::{EventEmitter, ModelContext, Task};
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, Role, StopReason,
};
use util::ResultExt as _;

/// A message in a [`Thread`].
pub struct Message {
    pub role: Role,
    pub text: String,
}

/// A thread of conversation with the LLM.
pub struct Thread {
    pub messages: Vec<Message>,
    pub pending_completion_tasks: Vec<Task<()>>,
}

impl Thread {
    pub fn new(_cx: &mut ModelContext<Self>) -> Self {
        Self {
            messages: Vec::new(),
            pending_completion_tasks: Vec::new(),
        }
    }

    pub fn stream_completion(
        &mut self,
        request: LanguageModelRequest,
        model: Arc<dyn LanguageModel>,
        cx: &mut ModelContext<Self>,
    ) {
        let task = cx.spawn(|this, mut cx| async move {
            let stream = model.stream_completion(request, &cx);
            let stream_completion = async {
                let mut events = stream.await?;
                let mut stop_reason = StopReason::EndTurn;

                while let Some(event) = events.next().await {
                    let event = event?;

                    this.update(&mut cx, |thread, cx| {
                        match event {
                            LanguageModelCompletionEvent::StartMessage { .. } => {
                                thread.messages.push(Message {
                                    role: Role::Assistant,
                                    text: String::new(),
                                });
                            }
                            LanguageModelCompletionEvent::Stop(reason) => {
                                stop_reason = reason;
                            }
                            LanguageModelCompletionEvent::Text(chunk) => {
                                if let Some(last_message) = thread.messages.last_mut() {
                                    if last_message.role == Role::Assistant {
                                        last_message.text.push_str(&chunk);
                                    }
                                }
                            }
                            LanguageModelCompletionEvent::ToolUse(_tool_use) => {}
                        }

                        cx.emit(ThreadEvent::StreamedCompletion);
                        cx.notify();
                    })?;

                    smol::future::yield_now().await;
                }

                anyhow::Ok(stop_reason)
            };

            let result = stream_completion.await;
            let _ = result.log_err();
        });

        self.pending_completion_tasks.push(task);
    }
}

#[derive(Debug, Clone)]
pub enum ThreadEvent {
    StreamedCompletion,
}

impl EventEmitter<ThreadEvent> for Thread {}