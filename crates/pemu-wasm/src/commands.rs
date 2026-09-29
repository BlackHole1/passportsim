//! The command list `pemu_call` dispatches against. Natively the `linkme` registry holds every
//! `#[command]`; on wasm32 it is empty until [`install`] hands it [`GENERATED`], which `build.rs`
//! writes into `OUT_DIR`.

use pemu_api::spec::CommandSpec;

include!(concat!(env!("OUT_DIR"), "/commands.rs"));

/// Installs [`GENERATED`] as the wasm32 registry, once. Natively this does nothing.
pub fn install() {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = pemu_api::registry::set_commands(&GENERATED);
    }
}

/// The registered command `name`, after [`install`].
pub fn find(name: &str) -> Option<&'static CommandSpec> {
    install();
    pemu_api::registry::find(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generated_list_is_the_linked_registry() {
        let mut generated: Vec<&str> = GENERATED.iter().map(|spec| spec.name).collect();
        let mut linked: Vec<&str> = pemu_api::registry::commands()
            .iter()
            .map(|spec| spec.name)
            .collect();
        generated.sort_unstable();
        linked.sort_unstable();
        assert!(!generated.is_empty());
        assert_eq!(
            generated, linked,
            "build.rs must list exactly the #[command] items linkme registers"
        );
    }

    #[test]
    fn find_answers_a_core_command_and_nothing_for_an_unknown_name() {
        assert_eq!(find("status").map(|spec| spec.name), Some("status"));
        assert!(find("no_such_command").is_none());
    }
}
