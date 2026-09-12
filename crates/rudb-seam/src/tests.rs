//! The contract of the seam machinery, which is the contract every seam inherits.

use rudb_common::ErrorCode;

use crate::context::Context;
use crate::policy::{ChoiceReason, Policy, PolicyMode};
use crate::registry::{Registries, Registry, RegistryView};
use crate::seam::SeamId;
use crate::settings::Settings;
use crate::strategy::{Determinism, Provenance, Strategy};

use std::sync::Arc;

/// A seam trait, standing in for a real one. Narrow, and it takes no scalar.
trait Toy: Strategy {
    fn run(&self) -> &'static str;
}

#[derive(Debug)]
struct Slow;

impl Strategy for Slow {
    fn name(&self) -> &'static str {
        "slow"
    }
    fn describe(&self) -> &'static str {
        "the obviously correct one"
    }
    fn provenance(&self) -> Provenance {
        Provenance::Reference
    }
    fn applicable(&self, _context: &Context<'_>) -> bool {
        true
    }
}

impl Toy for Slow {
    fn run(&self) -> &'static str {
        "slow"
    }
}

#[derive(Debug)]
struct Fast;

impl Strategy for Fast {
    fn name(&self) -> &'static str {
        "fast"
    }
    fn describe(&self) -> &'static str {
        "the one somebody published"
    }
    fn provenance(&self) -> Provenance {
        Provenance::Paper { title: "Something Unchained", venue: "DaMoN", year: 2024 }
    }
    fn applicable(&self, context: &Context<'_>) -> bool {
        context.estimated_rows().is_some_and(|rows| rows > 1000)
    }
    fn deterministic(&self) -> Determinism {
        Determinism::PerThreadCount
    }
}

impl Toy for Fast {
    fn run(&self) -> &'static str {
        "fast"
    }
}

fn registry() -> Registry<dyn Toy> {
    Registry::<dyn Toy>::builder(SeamId::HashTable)
        .reference(Box::new(Slow))
        .alternative(Box::new(Fast))
        .default_is("fast")
        .build()
}

#[test]
fn the_default_runs_when_it_is_applicable() {
    let registry = registry();
    let settings = Settings::new();
    let context = Context::new(SeamId::HashTable, &settings).with_estimated_rows(10_000);

    let chosen = registry.choose(&context).unwrap();
    assert_eq!(chosen.run(), "fast");
    assert_eq!(chosen.reason(), ChoiceReason::Default);
}

#[test]
fn a_default_that_cannot_run_falls_back_and_says_so() {
    let registry = registry();
    let settings = Settings::new();
    let context = Context::new(SeamId::HashTable, &settings).with_estimated_rows(10);

    let chosen = registry.choose(&context).unwrap();
    assert_eq!(chosen.run(), "slow");
    assert_eq!(chosen.reason(), ChoiceReason::Fallback);
}

#[test]
fn reference_mode_runs_the_reference_however_big_the_input() {
    let registry = registry();
    let settings = Settings::reference();
    let context = Context::new(SeamId::HashTable, &settings).with_estimated_rows(10_000_000);

    let chosen = registry.choose(&context).unwrap();
    assert_eq!(chosen.run(), "slow");
    assert_eq!(chosen.reason(), ChoiceReason::Reference);
}

#[test]
fn reference_mode_beats_a_pin() {
    let mut settings = Settings::new();
    settings.pin(SeamId::HashTable, "fast");
    settings.set_mode(PolicyMode::Reference);

    assert_eq!(settings.policy_for(SeamId::HashTable), Policy::Reference);
}

#[test]
fn a_pin_beats_the_default_rule() {
    let registry = registry();
    let mut settings = Settings::new();
    settings.pin(SeamId::HashTable, "slow");
    let context = Context::new(SeamId::HashTable, &settings).with_estimated_rows(10_000);

    let chosen = registry.choose(&context).unwrap();
    assert_eq!(chosen.run(), "slow");
    assert_eq!(chosen.reason(), ChoiceReason::Pinned);
}

#[test]
fn a_pin_on_a_name_nobody_registered_is_an_error_with_the_list_in_it() {
    let registry = registry();
    let mut settings = Settings::new();
    settings.pin(SeamId::HashTable, "unchained");
    let context = Context::new(SeamId::HashTable, &settings);

    let error = registry.choose(&context).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Catalog);
    assert!(error.message().contains("slow"), "{}", error.message());
    assert!(error.message().contains("fast"), "{}", error.message());
}

#[test]
fn a_pin_on_something_that_cannot_run_here_is_an_error_rather_than_a_quiet_fall_back() {
    let registry = registry();
    let mut settings = Settings::new();
    settings.pin(SeamId::HashTable, "fast");
    let context = Context::new(SeamId::HashTable, &settings).with_estimated_rows(10);

    let error = registry.choose(&context).unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidInput);
}

#[test]
fn the_adaptive_policy_says_which_milestone_owes_it() {
    let registry = registry();
    let mut settings = Settings::new();
    settings.set_mode(PolicyMode::Adaptive);
    let context = Context::new(SeamId::HashTable, &settings);

    let error = registry.choose(&context).unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotImplemented);
    assert!(error.message().contains("F10"), "{}", error.message());
}

#[test]
fn choosing_with_the_wrong_seams_context_is_caught() {
    let registry = registry();
    let settings = Settings::new();
    let context = Context::new(SeamId::Sort, &settings);

    let error = registry.choose(&context).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
}

#[test]
fn a_registry_with_no_default_named_defaults_to_the_reference() {
    let registry: Registry<dyn Toy> = Registry::<dyn Toy>::builder(SeamId::HashTable)
        .reference(Box::new(Slow))
        .alternative(Box::new(Fast))
        .build();
    assert_eq!(registry.default().name(), "slow");
    assert_eq!(registry.reference().name(), "slow");
}

#[test]
#[should_panic(expected = "without a reference")]
fn a_registry_without_a_reference_does_not_build() {
    let _: Registry<dyn Toy> =
        Registry::<dyn Toy>::builder(SeamId::HashTable).alternative(Box::new(Fast)).build();
}

#[test]
#[should_panic(expected = "two entries called")]
fn two_implementations_with_one_name_do_not_build() {
    let _: Registry<dyn Toy> = Registry::<dyn Toy>::builder(SeamId::HashTable)
        .reference(Box::new(Slow))
        .alternative(Box::new(Slow))
        .build();
}

#[test]
#[should_panic(expected = "not registered")]
fn a_default_nobody_registered_does_not_build() {
    let _: Registry<dyn Toy> = Registry::<dyn Toy>::builder(SeamId::HashTable)
        .reference(Box::new(Slow))
        .default_is("unchained")
        .build();
}

#[test]
fn the_rows_say_which_is_the_reference_and_which_is_the_default() {
    let registry = registry();
    let rows = registry.rows();
    assert_eq!(rows.len(), 2);

    assert_eq!(rows[0].name, "slow");
    assert!(rows[0].is_reference);
    assert!(!rows[0].is_default);
    assert_eq!(rows[0].determinism, Determinism::Exact);

    assert_eq!(rows[1].name, "fast");
    assert!(!rows[1].is_reference);
    assert!(rows[1].is_default);
    assert_eq!(rows[1].determinism, Determinism::PerThreadCount);
}

#[test]
fn every_seam_has_a_name_that_round_trips_and_a_milestone() {
    for seam in SeamId::ALL {
        assert_eq!(SeamId::from_name(seam.name()), Some(*seam));
        assert!(!seam.describe().is_empty());
        let milestone = seam.milestone();
        assert!(milestone.starts_with('F'), "{seam} is owed by {milestone}");
    }
}

#[test]
fn the_seam_list_has_no_duplicates() {
    let mut names: Vec<&str> = SeamId::ALL.iter().map(|seam| seam.name()).collect();
    let before = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), before);
}

#[test]
fn a_setting_round_trips_through_the_name_a_user_types() {
    let mut settings = Settings::new();
    assert_eq!(settings.get("seam.hash.table").as_deref(), Some("default"));

    settings.set("seam.hash.table", "unchained").unwrap();
    assert_eq!(settings.get("seam.hash.table").as_deref(), Some("unchained"));
    assert_eq!(settings.pinned(SeamId::HashTable), Some("unchained"));

    settings.set("hash.table", "default").unwrap();
    assert_eq!(settings.pinned(SeamId::HashTable), None);
}

#[test]
fn the_policy_is_set_through_the_same_door_as_everything_else() {
    let mut settings = Settings::new();
    settings.set("seam.policy", "reference").unwrap();
    assert_eq!(settings.mode(), PolicyMode::Reference);
    assert_eq!(settings.get("seam.policy").as_deref(), Some("reference"));

    let error = settings.set("seam.policy", "clever").unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidInput);
}

#[test]
fn a_hint_pins_as_many_seams_as_it_names_and_only_for_the_one_query() {
    let mut settings = Settings::new();
    settings.hint("hash.table(unchained) sort(radix)").unwrap();
    assert_eq!(settings.pinned(SeamId::HashTable), Some("unchained"));
    assert_eq!(settings.pinned(SeamId::Sort), Some("radix"));

    // Commas rather than spaces, and quotes around the value, are both what somebody who has just
    // written a `SET` will type.
    let mut commas = Settings::new();
    commas.hint("hash.table('unchained'), policy(reference)").unwrap();
    assert_eq!(commas.pinned(SeamId::HashTable), Some("unchained"));
    assert_eq!(commas.mode(), PolicyMode::Reference);
}

#[test]
fn a_hint_that_is_not_written_as_a_call_says_so() {
    let mut settings = Settings::new();
    let error = settings.hint("hash.table=unchained").unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    assert!(error.message().contains("seam(implementation)"), "{}", error.message());

    let unclosed = settings.hint("hash.table(unchained").unwrap_err();
    assert_eq!(unclosed.code(), ErrorCode::InvalidInput);

    // The name is checked the same way it is checked on the way in from a `SET`, because it is the
    // same function doing the checking.
    let mistyped = settings.hint("hash.tabel(unchained)").unwrap_err();
    assert_eq!(mistyped.code(), ErrorCode::Catalog);
}

#[test]
fn a_mistyped_seam_name_is_an_error_rather_than_a_setting_nobody_reads() {
    let mut settings = Settings::new();
    let error = settings.set("seam.hash.tabel", "unchained").unwrap_err();
    assert_eq!(error.code(), ErrorCode::Catalog);
}

#[test]
fn the_registries_list_says_what_is_not_built_yet() {
    let mut registries = Registries::new();
    assert_eq!(registries.unregistered().len(), SeamId::ALL.len());

    registries.add(Arc::new(registry()) as Arc<dyn RegistryView>);
    assert!(registries.has(SeamId::HashTable));
    assert_eq!(registries.unregistered().len(), SeamId::ALL.len() - 1);
    assert_eq!(registries.rows().len(), 2);
}

#[test]
#[should_panic(expected = "registered twice")]
fn one_seam_cannot_have_two_registries() {
    let mut registries = Registries::new();
    registries.add(Arc::new(registry()) as Arc<dyn RegistryView>);
    registries.add(Arc::new(registry()) as Arc<dyn RegistryView>);
}

#[test]
fn provenance_reads_the_way_explain_prints_it() {
    assert_eq!(Provenance::Reference.to_string(), "reference");
    assert_eq!(Provenance::Ours.to_string(), "ours");
    assert_eq!(
        Provenance::Paper { title: "Something Unchained", venue: "DaMoN", year: 2024 }.to_string(),
        "Something Unchained, DaMoN 2024"
    );
}
