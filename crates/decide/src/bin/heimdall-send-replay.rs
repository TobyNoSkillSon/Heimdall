//! Bounded JSONL replay of the application's actual send path. No profile access.
use anyhow::{ensure, Result};
use decide::{
    send::{self, Conversation},
    D1,
};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Write},
    time::Instant,
};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len() == 5, "LIBRARY MODEL PROJECTOR WINDOW");
    ensure!(args[4] == "2000", "production window is 2000 tokens");
    let started = Instant::now();
    let mut model = D1::load(&args[1], &args[2])?;
    let mut context = Conversation::default();
    println!(
        "{}",
        json!({"ready":{"window":2000,"cache_supported":false,"runtime":model.version()},"load_ms":started.elapsed().as_secs_f64()*1000.})
    );
    io::stdout().flush()?;
    for line in io::stdin().lock().lines() {
        ensure!(
            started.elapsed().as_secs() < 240,
            "four-minute replay budget exhausted"
        );
        let req: Value = serde_json::from_str(&line?)?;
        let id = req["id"].clone();
        let now = req["at_ns"].as_u64().unwrap_or(0);
        let text = req["text"].as_str().unwrap_or("");
        let speaker = req["speaker"].as_str().unwrap_or("User");
        let result = match req["op"].as_str().unwrap_or("final") {
            "question" => {
                let input:decide::DecisionInput=serde_json::from_value(req["input"].clone())?;
                let clock=Instant::now();
                let (answers,tokens)=model.answer(&input,0.)?;
                json!({"answers":answers,"tokens":tokens,"request":model.last_question_request().cloned(),"ms":clock.elapsed().as_secs_f64()*1000.})
            }
            "reset" => {
                context = Conversation::default();
                json!({"reset":true})
            }
            "history" => {
                context.append(speaker, text, false);
                json!({"history":true})
            }
            "answer" => {
                context.final_answer(text);
                if req["spoken"].as_bool() == Some(true) {
                    context.finished_ns = Some(now);
                }
                json!({"answer":true})
            }
            "typed" => {
                context.typed(text);
                json!({"trigger":"typed"})
            }
            "final" | "measure" => {
                context.append(speaker, text, true);
                let snapshot = context.snapshot();
                let clock = Instant::now();
                let (fit, prediction) = model.send_decision(&snapshot)?;
                context.trim_history(&snapshot, fit.dropped_turns);
                let explicit =
                    send::explicit_trigger(text, speaker == "User", now, context.finished_ns);
                let trigger = explicit.or_else(|| {
                    (prediction.choice == send::Choice::Send).then_some(send::Trigger::D1)
                });
                let ms = clock.elapsed().as_secs_f64() * 1000.;
                let pending = context.pending_text();
                if trigger.is_some() {
                    context.sent(snapshot.through);
                }
                json!({"fit":fit,"prediction":prediction,"trigger":trigger,"outcome":if trigger.is_some(){"send"}else{prediction.choice.key()},"pending":pending,"ms":ms})
            }
            _ => anyhow::bail!("unknown replay operation"),
        };
        println!("{}", json!({"id":id,"result":result}));
        io::stdout().flush()?;
    }
    Ok(())
}
