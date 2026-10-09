//! Explicit keyboard ownership, independent of ordinary voice allocation.
use anyhow::Result;
use std::path::Path;

#[cfg(target_os = "macos")]
mod macos;

#[derive(Clone, Copy, Debug)]
pub enum CapsCommand {
    Status,
    Setup,
    Run,
    Disable,
}

pub async fn caps_command(directory: &Path, command: CapsCommand) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::command(directory, command).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (directory, command);
        anyhow::bail!(
            "Caps hardware integration is not implemented on this platform; voice allocation and voice-control off remain available"
        )
    }
}

/// A physical edge is a request; only confirmed lifecycle drives the indicator.
#[derive(Default)]
struct Edges {
    down: std::collections::BTreeSet<usize>,
}
impl Edges {
    fn update(&mut self, device: usize, down: bool) -> bool {
        if down {
            self.down.insert(device)
        } else {
            self.down.remove(&device);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_or_release_events_do_not_toggle_and_devices_are_independent() {
        let mut edges = Edges::default();
        assert!(edges.update(1, true));
        assert!(!edges.update(1, true));
        assert!(edges.update(2, true));
        assert!(!edges.update(1, false));
        assert!(edges.update(1, true));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mapping {
    Other,
    OwnedShape,
    ForeignCaps,
}

struct MappingPlan {
    retain: Vec<usize>,
    append: bool,
    unchanged: bool,
}

fn mapping_plan(current: &[Mapping], owned: bool, acquire: bool) -> Result<MappingPlan> {
    if acquire {
        let caps = current
            .iter()
            .filter(|entry| **entry != Mapping::Other)
            .collect::<Vec<_>>();
        if owned {
            anyhow::ensure!(
                caps == vec![&Mapping::OwnedShape],
                "Caps ownership changed externally; refusing to overwrite mappings"
            );
            return Ok(MappingPlan {
                retain: (0..current.len()).collect(),
                append: false,
                unchanged: true,
            });
        }
        anyhow::ensure!(
            caps.is_empty(),
            "Caps is already remapped by another owner; release it before enabling Codex Caps"
        );
        Ok(MappingPlan {
            retain: (0..current.len()).collect(),
            append: true,
            unchanged: false,
        })
    } else {
        Ok(MappingPlan {
            retain: current
                .iter()
                .enumerate()
                .filter_map(|(i, entry)| (!owned || *entry != Mapping::OwnedShape).then_some(i))
                .collect(),
            append: false,
            unchanged: !owned,
        })
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    #[test]
    fn owned_entry_only_cleanup_preserves_other_and_replaced_caps_mappings() {
        let current = [
            Mapping::Other,
            Mapping::OwnedShape,
            Mapping::Other,
            Mapping::ForeignCaps,
        ];
        let plan = mapping_plan(&current, true, false).unwrap();
        assert_eq!(plan.retain, vec![0, 2, 3]);
        assert!(!plan.append);
        assert_eq!(
            mapping_plan(&current, false, false).unwrap().retain,
            vec![0, 1, 2, 3]
        );
    }
    #[test]
    fn same_mapping_from_another_owner_is_a_conflict_not_ownership() {
        assert!(mapping_plan(&[Mapping::OwnedShape], false, true).is_err());
        assert!(mapping_plan(&[Mapping::ForeignCaps], false, true).is_err());
        assert!(mapping_plan(&[Mapping::OwnedShape, Mapping::ForeignCaps], true, true).is_err());
        assert!(mapping_plan(&[], true, true).is_err());
    }
    #[test]
    fn acquire_preserves_order_and_reconcile_never_appends_twice() {
        let plan = mapping_plan(&[Mapping::Other, Mapping::Other], false, true).unwrap();
        assert_eq!(plan.retain, vec![0, 1]);
        assert!(plan.append);
        let plan = mapping_plan(&[Mapping::Other, Mapping::OwnedShape], true, true).unwrap();
        assert!(plan.unchanged);
        assert!(!plan.append);
    }
}
