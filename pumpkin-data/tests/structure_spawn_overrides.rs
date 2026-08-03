//! Regression coverage for generated structure `spawn_overrides` data.
//!
//! `pumpkin-codegen` is excluded from the workspace (`Cargo.toml`'s
//! `exclude = ["pumpkin-codegen"]`), so its own unit tests never run under
//! `cargo nextest run`. These tests assert against the committed generated
//! constants instead, which is what the runtime actually consumes.
//!
//! Vanilla 26.2 references: `Structures.java`, `StructureSpawnOverride.java`,
//! `MobSpawnSettings.java`, and `ChunkGenerator.java`.

#![cfg(all(feature = "structures", feature = "entity"))]

use pumpkin_data::entity::{EntityType, MobCategory};
use pumpkin_data::structures::{
    Structure, StructureKeys, StructureSpawnBoundingBox, StructureSpawnCategory,
    StructureSpawnEntry, StructureSpawnOverride,
};

/// Every structure key, so the data-wide invariants below cannot silently skip a
/// structure that upstream adds overrides to later.
const ALL_STRUCTURE_KEYS: [StructureKeys; 34] = [
    StructureKeys::AncientCity,
    StructureKeys::BastionRemnant,
    StructureKeys::BuriedTreasure,
    StructureKeys::DesertPyramid,
    StructureKeys::EndCity,
    StructureKeys::Fortress,
    StructureKeys::Igloo,
    StructureKeys::JunglePyramid,
    StructureKeys::Mansion,
    StructureKeys::Mineshaft,
    StructureKeys::MineshaftMesa,
    StructureKeys::Monument,
    StructureKeys::NetherFossil,
    StructureKeys::OceanRuinCold,
    StructureKeys::OceanRuinWarm,
    StructureKeys::PillagerOutpost,
    StructureKeys::RuinedPortal,
    StructureKeys::RuinedPortalDesert,
    StructureKeys::RuinedPortalJungle,
    StructureKeys::RuinedPortalMountain,
    StructureKeys::RuinedPortalNether,
    StructureKeys::RuinedPortalOcean,
    StructureKeys::RuinedPortalSwamp,
    StructureKeys::Shipwreck,
    StructureKeys::ShipwreckBeached,
    StructureKeys::Stronghold,
    StructureKeys::SwampHut,
    StructureKeys::TrailRuins,
    StructureKeys::TrialChambers,
    StructureKeys::VillageDesert,
    StructureKeys::VillagePlains,
    StructureKeys::VillageSavanna,
    StructureKeys::VillageSnowy,
    StructureKeys::VillageTaiga,
];

/// Vanilla `MobCategory` order (`MobCategory.java:14-21`), used to assert the
/// generated slices stay in a deterministic, vanilla-matching order.
const CATEGORY_ORDER: [StructureSpawnCategory; 8] = [
    StructureSpawnCategory::Monster,
    StructureSpawnCategory::Creature,
    StructureSpawnCategory::Ambient,
    StructureSpawnCategory::Axolotls,
    StructureSpawnCategory::UndergroundWaterCreature,
    StructureSpawnCategory::WaterCreature,
    StructureSpawnCategory::WaterAmbient,
    StructureSpawnCategory::Misc,
];

/// Looks up one category the way runtime code has to: a linear scan over the
/// generated slice. Returns `None` for an absent category, mirroring vanilla's
/// `Map::get` returning `null`.
fn find_override(
    structure: &'static Structure,
    category: StructureSpawnCategory,
) -> Option<&'static StructureSpawnOverride> {
    structure
        .spawn_overrides
        .iter()
        .find(|entry| entry.category == category)
}

fn category_index(category: StructureSpawnCategory) -> usize {
    CATEGORY_ORDER
        .iter()
        .position(|candidate| *candidate == category)
        .expect("every generated category must be part of the vanilla MobCategory order")
}

#[test]
fn fortress_and_outpost_spawn_overrides_preserve_weighted_entries() {
    let fortress = Structure::get(&StructureKeys::Fortress);
    let monster = find_override(fortress, StructureSpawnCategory::Monster)
        .expect("fortress must override the monster category");

    // `NetherFortressStructure` spawns are piece-scoped in vanilla.
    assert_eq!(monster.bounding_box, StructureSpawnBoundingBox::Piece);
    assert_eq!(
        monster.spawns,
        &[
            StructureSpawnEntry {
                r#type: "minecraft:blaze",
                min_count: 2,
                max_count: 3,
                weight: 10,
            },
            StructureSpawnEntry {
                r#type: "minecraft:zombified_piglin",
                min_count: 4,
                max_count: 4,
                weight: 5,
            },
            StructureSpawnEntry {
                r#type: "minecraft:wither_skeleton",
                min_count: 5,
                max_count: 5,
                weight: 8,
            },
            StructureSpawnEntry {
                r#type: "minecraft:skeleton",
                min_count: 5,
                max_count: 5,
                weight: 2,
            },
            StructureSpawnEntry {
                r#type: "minecraft:magma_cube",
                min_count: 4,
                max_count: 4,
                weight: 3,
            },
        ]
    );

    let outpost = Structure::get(&StructureKeys::PillagerOutpost);
    let outpost_monster = find_override(outpost, StructureSpawnCategory::Monster)
        .expect("pillager outpost must override the monster category");
    assert_eq!(
        outpost_monster.bounding_box,
        StructureSpawnBoundingBox::Full
    );
    assert_eq!(
        outpost_monster.spawns,
        &[StructureSpawnEntry {
            r#type: "minecraft:pillager",
            min_count: 1,
            max_count: 1,
            weight: 1,
        }]
    );
}

#[test]
fn monument_and_swamp_hut_preserve_all_categories_and_bounds() {
    let monument = Structure::get(&StructureKeys::Monument);
    assert_eq!(monument.spawn_overrides.len(), 3);
    assert_eq!(
        find_override(monument, StructureSpawnCategory::Monster)
            .expect("monument must override the monster category")
            .spawns,
        &[StructureSpawnEntry {
            r#type: "minecraft:guardian",
            min_count: 2,
            max_count: 4,
            weight: 1,
        }]
    );
    // Vanilla suppresses these two categories inside the monument bounds.
    for category in [
        StructureSpawnCategory::Axolotls,
        StructureSpawnCategory::UndergroundWaterCreature,
    ] {
        let override_data =
            find_override(monument, category).expect("monument must list this category");
        assert_eq!(override_data.bounding_box, StructureSpawnBoundingBox::Full);
        assert!(override_data.spawns.is_empty());
    }

    let swamp_hut = Structure::get(&StructureKeys::SwampHut);
    assert_eq!(swamp_hut.spawn_overrides.len(), 2);
    assert_eq!(
        find_override(swamp_hut, StructureSpawnCategory::Creature)
            .expect("swamp hut must override the creature category")
            .spawns,
        &[StructureSpawnEntry {
            r#type: "minecraft:cat",
            min_count: 1,
            max_count: 1,
            weight: 1,
        }]
    );
    assert_eq!(
        find_override(swamp_hut, StructureSpawnCategory::Monster)
            .expect("swamp hut must override the monster category")
            .spawns,
        &[StructureSpawnEntry {
            r#type: "minecraft:witch",
            min_count: 1,
            max_count: 1,
            weight: 1,
        }]
    );
    for override_data in swamp_hut.spawn_overrides {
        assert_eq!(override_data.bounding_box, StructureSpawnBoundingBox::Piece);
    }
}

#[test]
fn empty_spawn_overrides_remain_distinct_from_absent_categories() {
    // A present category with an empty pool suppresses natural spawning; an absent
    // category falls through to biome data. `ChunkGenerator.java:368` skips only on
    // the absent case (`if (override == null) continue;`), so the two must not be
    // collapsed into one representation.
    for (key, bounding_box) in [
        (StructureKeys::AncientCity, StructureSpawnBoundingBox::Full),
        (
            StructureKeys::TrialChambers,
            StructureSpawnBoundingBox::Piece,
        ),
    ] {
        let structure = Structure::get(&key);
        assert_eq!(structure.spawn_overrides.len(), CATEGORY_ORDER.len());
        for category in CATEGORY_ORDER {
            let override_data = find_override(structure, category)
                .expect("structure must list every category explicitly");
            assert_eq!(override_data.bounding_box, bounding_box);
            assert!(
                override_data.spawns.is_empty(),
                "{category:?} must stay a present-but-empty suppression entry"
            );
        }
    }

    // Bastion remnant has no override map at all, which is a different thing from
    // having one full of empty pools.
    let bastion = Structure::get(&StructureKeys::BastionRemnant);
    assert!(bastion.spawn_overrides.is_empty());
    for category in CATEGORY_ORDER {
        assert!(find_override(bastion, category).is_none());
    }
}

#[test]
fn spawn_overrides_are_deterministic_and_well_formed() {
    for key in ALL_STRUCTURE_KEYS {
        let structure = Structure::get(&key);

        // At most one entry per category, emitted in vanilla `MobCategory` order,
        // which is what makes the linear-scan slice a faithful stand-in for a map.
        let mut previous: Option<usize> = None;
        for override_data in structure.spawn_overrides {
            let index = category_index(override_data.category);
            if let Some(previous_index) = previous {
                assert!(
                    previous_index < index,
                    "{key:?} spawn_overrides must be strictly ordered by MobCategory"
                );
            }
            previous = Some(index);

            // `MobSpawnSettings.SpawnerData`'s codec rejects non-positive counts and
            // `minCount > maxCount` (`MobSpawnSettings.java:74-80`).
            for entry in override_data.spawns {
                assert!(entry.min_count > 0, "{key:?}: minCount must be positive");
                assert!(entry.max_count > 0, "{key:?}: maxCount must be positive");
                assert!(
                    entry.min_count <= entry.max_count,
                    "{key:?}: minCount must not exceed maxCount"
                );
                assert!(entry.weight >= 0, "{key:?}: weight must be non-negative");
            }
        }

        assert!(
            structure.spawn_overrides.len() <= CATEGORY_ORDER.len(),
            "{key:?} cannot have more overrides than there are mob categories"
        );
    }
}

#[test]
fn no_spawn_entry_resolves_to_a_misc_category_entity() {
    // `MobSpawnSettings.java:82-84`: the `SpawnerData` canonical constructor rewrites
    // any `MobCategory.MISC` entity type to `PIG`, so a MISC type can never survive
    // into loaded data. Codegen performs that substitution up front; assert the
    // generated result is already consistent with it.
    for key in ALL_STRUCTURE_KEYS {
        let structure = Structure::get(&key);
        for override_data in structure.spawn_overrides {
            for entry in override_data.spawns {
                let entity = EntityType::from_name(entry.r#type)
                    .unwrap_or_else(|| panic!("{key:?}: unknown entity type {}", entry.r#type));
                assert_ne!(
                    *entity.category,
                    MobCategory::MISC,
                    "{key:?}: {} is MISC and must have been remapped to minecraft:pig",
                    entry.r#type
                );
            }
        }
    }
}
