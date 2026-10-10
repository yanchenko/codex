use super::*;

fn setup(voices: &[&str]) -> Registry {
    Registry::default()
        .transition(Operation::Configure {
            provider: "openai".into(),
            voices: voices.iter().map(|s| (*s).into()).collect(),
        })
        .unwrap()
}
fn register(state: &mut Registry) -> Call {
    let call = Call {
        instance: Uuid::new_v4(),
        owner: Uuid::new_v4().to_string(),
        generation: 1,
    };
    *state = state
        .transition(Operation::Register {
            instance: call.instance,
            owner: call.owner.clone(),
            provider: "openai".into(),
        })
        .unwrap();
    call
}
fn reserve(state: &mut Registry, call: &Call) {
    *state = state
        .transition(Operation::Reserve {
            call: call.clone(),
            epoch: state.epoch,
        })
        .unwrap();
}
#[test]
fn occupied_voices_skipped_and_exhaustion_never_duplicates() {
    let mut state = setup(&["alloy", "ash"]);
    let first = register(&mut state);
    let second = register(&mut state);
    let third = register(&mut state);
    reserve(&mut state, &first);
    reserve(&mut state, &second);
    assert_eq!(state.leases[&first.instance].voice, "alloy");
    assert_eq!(state.leases[&second.instance].voice, "ash");
    assert!(
        state
            .transition(Operation::Reserve {
                call: third.clone(),
                epoch: state.epoch
            })
            .is_err()
    );
    state = state
        .transition(Operation::Retired { call: second })
        .unwrap();
    reserve(&mut state, &third);
    assert_eq!(state.leases[&third.instance].voice, "ash");
}
#[test]
fn global_off_invalidates_inflight_requests_and_delayed_start_ack() {
    let mut state = setup(&["alloy", "ash"]);
    let first = register(&mut state);
    let second = register(&mut state);
    reserve(&mut state, &first);
    let previous = state.epoch;
    state = state.transition(Operation::GlobalOff).unwrap();
    assert!(
        state
            .transition(Operation::Active {
                call: first.clone(),
                epoch: previous
            })
            .is_err()
    );
    assert!(
        state
            .transition(Operation::Reserve {
                call: second.clone(),
                epoch: state.epoch
            })
            .is_err()
    );
    state = state
        .transition(Operation::Retired { call: first })
        .unwrap();
    assert!(!state.stopping);
    assert!(
        state
            .transition(Operation::Reserve {
                call: second.clone(),
                epoch: previous
            })
            .is_err()
    );
    reserve(&mut state, &second);
}
#[test]
fn restart_and_disconnect_quarantine_until_actual_retirement() {
    let mut state = setup(&["alloy"]);
    let first = register(&mut state);
    reserve(&mut state, &first);
    state = state
        .transition(Operation::Active {
            call: first.clone(),
            epoch: state.epoch,
        })
        .unwrap();
    state.recover().unwrap();
    assert!(state.confirmed_active);
    assert_eq!(state.leases[&first.instance].phase, Phase::Unknown);
    assert!(
        state
            .transition(Operation::Forget {
                instance: first.instance
            })
            .is_err()
    );
    let second = register(&mut state);
    assert!(
        state
            .transition(Operation::Reserve {
                call: second.clone(),
                epoch: state.epoch
            })
            .is_err()
    );
    state = state
        .transition(Operation::Retired { call: first })
        .unwrap();
    reserve(&mut state, &second);
}
#[test]
fn wrong_owner_and_duplicate_generation_cannot_release_or_start() {
    let mut state = setup(&["alloy"]);
    let first = register(&mut state);
    reserve(&mut state, &first);
    let mut wrong = first.clone();
    wrong.owner = "wrong".into();
    assert!(
        state
            .transition(Operation::Retired { call: wrong })
            .is_err()
    );
    assert!(
        state
            .transition(Operation::Configure {
                provider: "openai".into(),
                voices: vec!["ash".into()]
            })
            .is_err()
    );
    let mut second = first.clone();
    second.generation = 2;
    assert!(
        state
            .transition(Operation::Reserve {
                call: second,
                epoch: state.epoch
            })
            .is_err()
    );
    let repeated = state
        .transition(Operation::Reserve {
            call: first,
            epoch: state.epoch,
        })
        .unwrap();
    assert_eq!(repeated.leases.len(), 1);
}
#[test]
fn global_off_cancels_queued_start_and_repeated_off_is_safe() {
    let mut state = setup(&["alloy"]);
    let first = register(&mut state);
    state = state
        .transition(Operation::Bind {
            instance: first.instance,
        })
        .unwrap();
    state = state
        .transition(Operation::Start { epoch: state.epoch })
        .unwrap();
    assert!(state.instances[&first.instance].start_requested);
    state = state.transition(Operation::GlobalOff).unwrap();
    assert!(!state.instances[&first.instance].start_requested);
    state = state.transition(Operation::GlobalOff).unwrap();
    assert!(!state.confirmed_active);
    state = state
        .transition(Operation::Disconnect {
            instance: first.instance,
        })
        .unwrap();
    assert!(state.bound.is_none());
    assert!(
        state
            .transition(Operation::Start { epoch: state.epoch })
            .is_err()
    );
}
#[test]
fn invalid_pools_fail_without_mutation() {
    let state = setup(&["alloy"]);
    for voices in [
        vec![],
        vec!["ash".into(), "ash".into()],
        vec!["bad voice".into()],
    ] {
        assert!(
            state
                .transition(Operation::Configure {
                    provider: "openai".into(),
                    voices
                })
                .is_err()
        );
    }
    assert_eq!(state.pools["openai"].voices, vec!["alloy"]);
}

#[test]
fn retired_generation_cannot_replay_after_new_call() {
    let mut state = setup(&["alloy"]);
    let first = register(&mut state);
    reserve(&mut state, &first);
    state = state
        .transition(Operation::Retired {
            call: first.clone(),
        })
        .unwrap();
    assert!(
        state
            .transition(Operation::Reserve {
                call: first.clone(),
                epoch: state.epoch
            })
            .is_err()
    );
    let mut next = first.clone();
    next.generation += 1;
    reserve(&mut state, &next);
    assert!(
        state
            .transition(Operation::Retired { call: first })
            .is_err()
    );
    assert_eq!(
        state.leases[&next.instance].call.generation,
        next.generation
    );
}

#[test]
fn corrupt_journal_cannot_recover_an_empty_pool_or_duplicate_voice() {
    let mut state = setup(&["alloy", "ash"]);
    let first = register(&mut state);
    let second = register(&mut state);
    reserve(&mut state, &first);
    reserve(&mut state, &second);
    let mut duplicate = state.clone();
    duplicate.leases.get_mut(&second.instance).unwrap().voice = "alloy".into();
    assert!(duplicate.recover().is_err());
    state.pools.get_mut("openai").unwrap().voices.clear();
    assert!(state.recover().is_err());
}
