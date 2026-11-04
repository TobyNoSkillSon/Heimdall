    use super::*;
    use crate::pipeline::plumbing_tests::rig;
    use std::time::Duration;
    #[test]
    fn command_wait_behind_the_largest_fitted_foreground_question() {
        let (_temp, mut app, _) = rig(false);
        app.models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
        tests::ready(&mut app);
        let mut input = tests::command_input("largest-foreground", &"Please inspect this build and report the failure. ".repeat(40));
        input.questions = vec![decide::Question::CommandCheck(
            decide::CommandSelection::TakeScreenshot,
        )];
        input.pending_transcript =
            "Please inspect the current build and report every failure. ".repeat(1000);
        input.context = (0..20)
            .map(|i| decide::Turn {
                id: i.to_string(),
                role: decide::Role::Owner,
                transcript:
                    "Please inspect the previous result and preserve the relevant context. "
                        .repeat(30),
                audio: decide::AudioClip::default(),
            })
            .collect();
        let mut waits = vec![];
        let mut totals = vec![];
        // The first requests after a load run cold and the app gives them its cold allowance (send_worker.rs `COLD`,
        // `COLD_REQUESTS` = 3, 10 s); this bound is the warm path's. Two rounds (four requests) spend it unmeasured,
        // then five are measured (3 Nov night check: round 1 waited 1,442 ms with the whole suite on the GPU, rounds 2
        // to 5 about 190 ms).
        for round in 0..7 {
            let owner = app.inference().unwrap();
            let (began, started) = crossbeam_channel::bounded(1);
            let input = input.clone();
            owner
                .task(2, move |s| {
                    began.send(()).unwrap();
                    let (_, tokens) = s.decider().unwrap().command_answer(&input).0.unwrap();
                    assert!(
                        tokens <= 2000 && tokens >= 1700,
                        "largest foreground tokens={tokens}"
                    );
                    Box::new(move |_| {
                        eprintln!("bounded foreground input={tokens} tokens");
                        Ok(())
                    })
                })
                .unwrap();
            started.recv_timeout(Duration::from_secs(3)).unwrap();
            let queued = Instant::now();
            let (finished, done) = crossbeam_channel::bounded(1);
            owner
                .task(2, move |s| {
                    let wait = queued.elapsed().as_secs_f64() * 1000.;
                    let input = tests::command_input("bounded-command", "look at this, the menu is cut off");
                    s.decider().unwrap().command_answer(&input).0.unwrap();
                    finished
                        .send((wait, queued.elapsed().as_secs_f64() * 1000.))
                        .unwrap();
                    Box::new(|_| Ok(()))
                })
                .unwrap();
            let (wait, total) = done.recv_timeout(Duration::from_secs(5)).unwrap();
            if round >= 2 {
                waits.push(wait);
                totals.push(total);
            }
            tests::drain(&mut app);
        }
        let worst = waits.iter().copied().fold(0., f64::max);
        let total = totals.iter().copied().fold(0., f64::max);
        eprintln!("FIX4 largest foreground: waits={waits:?} ms; totals={totals:?} ms; worst wait={worst:.1}ms, worst with answer={total:.1}ms");
        // Match the app diagnostic slow-decision boundary; token assertions
        // above separately enforce the hard context cap.
        assert!(
            total < 500.,
            "bounded foreground plus a command check exceeded the 500ms diagnostic boundary: {total:.1}"
        );
    }
