use pumpkin_data::BlockState;
use pumpkin_data::chunk_gen_settings::GenerationSettings;
use pumpkin_data::dimension::Dimension;
use pumpkin_data::noise_router::{
    END_BASE_NOISE_ROUTER, NETHER_BASE_NOISE_ROUTER, OVERWORLD_BASE_NOISE_ROUTER,
};

use super::noise::router::proto_noise_router::ProtoNoiseRouters;
use crate::generation::proto_chunk::TerrainCache;
use crate::generation::{GlobalRandomConfig, Seed};

pub mod structure_finder;

pub trait GeneratorInit {
    fn new(seed: Seed, dimension: Dimension) -> Self;
}

use pumpkin_data::structures::{StructurePlacementCalculator, StructureSet};
use rustc_hash::FxHashMap;

pub mod flat;

#[derive(Clone, Debug)]
pub struct FlatLayer {
    pub block: String,
    pub height: i32,
}

pub enum WorldGenerator {
    Noise(Box<VanillaGenerator>),
    Flat(flat::FlatGenerator),
}

impl WorldGenerator {
    #[must_use]
    pub const fn dimension(&self) -> &Dimension {
        match self {
            Self::Noise(noise_gen) => &noise_gen.dimension,
            Self::Flat(flat_gen) => &flat_gen.dimension,
        }
    }

    #[must_use]
    pub const fn seed(&self) -> u64 {
        match self {
            Self::Noise(noise_gen) => noise_gen.random_config.seed,
            Self::Flat(flat_gen) => flat_gen.seed,
        }
    }

    #[must_use]
    pub const fn global_structure_cache(
        &self,
    ) -> Option<&crate::generation::structure::placement::GlobalStructureCache> {
        match self {
            Self::Noise(noise_gen) => Some(&noise_gen.global_structure_cache),
            Self::Flat(_) => None,
        }
    }
}

pub struct VanillaGenerator {
    pub random_config: GlobalRandomConfig,
    pub base_router: ProtoNoiseRouters,
    pub dimension: Dimension,
    pub settings: &'static GenerationSettings,
    pub biome_mixer_seed: i64,

    pub terrain_cache: TerrainCache,

    pub default_block: &'static BlockState,

    pub global_structure_cache: crate::generation::structure::placement::GlobalStructureCache,
    pub structure_calculator: StructurePlacementCalculator,
    /// Allowed biome ids per structure set, keyed by the set's index in
    /// [`StructureSet::ALL`].
    ///
    /// Keyed by the original `StructureSet::ALL` index (not by a position in
    /// [`Self::possible_structure_sets`]) so both stay valid independently.
    pub structure_allowed_biomes: FxHashMap<usize, Vec<u16>>,
    /// Indices into [`StructureSet::ALL`] of the sets that can actually place
    /// something in this dimension.
    ///
    /// Vanilla filters the structure sets once, when the generator's structure
    /// state is built, instead of retrying impossible sets per chunk:
    /// `ChunkGeneratorStructureState.createForNormal` keeps only the sets whose
    /// candidate structures share a biome with the dimension's biome source
    /// (`/root/Vanilla/src/net/minecraft/world/level/chunk/ChunkGeneratorStructureState.java:63-75`),
    /// and every later pass iterates `possibleStructureSets()`
    /// (`:88-107`, and `ChunkGenerator.createStructures`
    /// `/root/Vanilla/src/net/minecraft/world/level/chunk/ChunkGenerator.java:605`).
    ///
    /// Without it the overworld keeps running the full placement + biome query
    /// for `NetherFossil`, the end for `Monument`, and so on: work that can only
    /// ever be thrown away.
    pub possible_structure_sets: Vec<usize>,
}

/// Vanilla `ChunkGeneratorStructureState.hasBiomesForStructureSet`
/// (`ChunkGeneratorStructureState.java:69-75`): a set is possible when any of its
/// candidate structures lists a biome that the dimension's biome source can
/// produce.
fn has_biomes_for_structure_set(allowed_biomes: &[u16], possible_biomes: &[u16]) -> bool {
    allowed_biomes
        .iter()
        .any(|biome| possible_biomes.contains(biome))
}

impl GeneratorInit for VanillaGenerator {
    fn new(seed: Seed, dimension: Dimension) -> Self {
        let settings = GenerationSettings::from_dimension(&dimension);
        let random_config = GlobalRandomConfig::new(seed.0, settings.legacy_random_source);

        // TODO: The generation settings contains (part of?) the noise routers too; do we keep the separate or
        // use only the generation settings?
        let base = if dimension == Dimension::OVERWORLD {
            OVERWORLD_BASE_NOISE_ROUTER
        } else if dimension == Dimension::THE_NETHER {
            NETHER_BASE_NOISE_ROUTER
        } else if dimension == Dimension::THE_END {
            END_BASE_NOISE_ROUTER
        } else {
            tracing::error!("Unsupported dimension for noise router: {:?}", dimension);
            OVERWORLD_BASE_NOISE_ROUTER
        };
        let terrain_cache = TerrainCache::from_random(&random_config);

        let default_block = settings.default_block;
        let base_router = ProtoNoiseRouters::generate(&base, &random_config);
        let biome_mixer_seed = crate::biome::hash_seed(seed.0);

        // Both the per-set biome lists and the dimension filter are computed
        // exactly once here: the filter is a set intersection, far too expensive
        // for the per-chunk structure passes.
        let possible_biomes = crate::biome::dimension_possible_biomes(&dimension);
        let mut structure_allowed_biomes = FxHashMap::default();
        let mut possible_structure_sets = Vec::new();
        for (i, set) in StructureSet::ALL.iter().enumerate() {
            let allowed_biomes =
                crate::generation::proto_chunk::ProtoChunk::get_allowed_biomes(set);
            if has_biomes_for_structure_set(&allowed_biomes, &possible_biomes) {
                possible_structure_sets.push(i);
            }
            structure_allowed_biomes.insert(i, allowed_biomes);
        }

        Self {
            random_config,
            base_router,
            dimension,
            settings,
            biome_mixer_seed,
            terrain_cache,
            default_block,
            global_structure_cache:
                crate::generation::structure::placement::GlobalStructureCache::new(),
            structure_calculator: StructurePlacementCalculator::new(seed.0 as i64),
            structure_allowed_biomes,
            possible_structure_sets,
        }
    }
}

impl VanillaGenerator {
    /// The structure sets that can place something in this dimension, paired with
    /// their original [`StructureSet::ALL`] index.
    ///
    /// The index is what [`Self::structure_allowed_biomes`] is keyed by, so the
    /// structure passes must carry it through rather than re-deriving a position
    /// from the filtered list.
    #[must_use]
    pub fn possible_structure_sets(
        &self,
    ) -> impl Iterator<Item = (usize, &'static StructureSet)> + '_ {
        self.possible_structure_sets
            .iter()
            .map(|&index| (index, &StructureSet::ALL[index]))
    }
}

#[cfg(test)]
mod tests {
    use pumpkin_data::dimension::Dimension;
    use pumpkin_data::structures::StructureSet;

    use super::{GeneratorInit, VanillaGenerator};
    use crate::generation::Seed;

    fn generator_for(dimension: Dimension) -> VanillaGenerator {
        VanillaGenerator::new(Seed(1_234_567), dimension)
    }

    /// Whether `set_name`'s set survived the dimension filter.
    ///
    /// Vanilla filters whole *sets*, not individual entries
    /// (`ChunkGeneratorStructureState.java:63-75`), so this is the granularity the
    /// expectations below are written at.
    fn keeps(dimension: Dimension, set_name: &str) -> bool {
        let expected = StructureSet::get(set_name).expect("known structure set");
        generator_for(dimension)
            .possible_structure_sets()
            .any(|(_, set)| std::ptr::eq(set, expected))
    }

    /// `ChunkGeneratorStructureState.hasBiomesForStructureSet`
    /// (`ChunkGeneratorStructureState.java:69-75`) drops a set when none of its
    /// structures' biomes occur in the dimension. `nether_fossils` in the
    /// overworld and `ocean_monuments` in the end are the two the development log
    /// showed being retried, and failing, for every chunk.
    #[test]
    fn dimension_filter_matches_vanilla_per_set() {
        // (set, keep in overworld, keep in nether, keep in end)
        let expected = [
            ("ancient_cities", true, false, false),
            ("buried_treasures", true, false, false),
            ("desert_pyramids", true, false, false),
            ("end_cities", false, false, true),
            ("igloos", true, false, false),
            ("jungle_temples", true, false, false),
            ("mineshafts", true, false, false),
            ("nether_complexes", false, true, false),
            ("nether_fossils", false, true, false),
            ("ocean_monuments", true, false, false),
            ("ocean_ruins", true, false, false),
            ("pillager_outposts", true, false, false),
            // The one mixed set: `minecraft:ruined_portals` holds both the
            // overworld variants and `ruined_portal_nether`, so vanilla's
            // set-level filter keeps it in both dimensions. The per-entry biome
            // check at the start/reference pass is what rejects the wrong
            // variant there.
            ("ruined_portals", true, true, false),
            ("shipwrecks", true, false, false),
            ("strongholds", true, false, false),
            ("swamp_huts", true, false, false),
            ("trail_ruins", true, false, false),
            ("trial_chambers", true, false, false),
            ("villages", true, false, false),
            ("woodland_mansions", true, false, false),
        ];

        // Guards against a new set being added to the registry without an
        // expectation here.
        assert_eq!(expected.len(), StructureSet::ALL.len());

        for (name, overworld, nether, end) in expected {
            assert_eq!(
                keeps(Dimension::OVERWORLD, name),
                overworld,
                "overworld / {name}"
            );
            assert_eq!(
                keeps(Dimension::THE_NETHER, name),
                nether,
                "nether / {name}"
            );
            assert_eq!(keeps(Dimension::THE_END, name), end, "end / {name}");
        }
    }

    /// Each dimension must drop a meaningful part of the registry; if a dimension
    /// still iterates everything, the per-chunk waste this filter exists to
    /// remove is back.
    #[test]
    fn every_dimension_filters_out_some_sets() {
        for (dimension, kept) in [
            (Dimension::OVERWORLD, 17),
            (Dimension::THE_NETHER, 3),
            (Dimension::THE_END, 1),
        ] {
            let generator = generator_for(dimension);
            assert_eq!(generator.possible_structure_sets.len(), kept);
            assert!(kept < StructureSet::ALL.len());
        }
    }

    /// The three structure passes index `structure_allowed_biomes` with the index
    /// yielded by `possible_structure_sets`, so that index must stay the
    /// `StructureSet::ALL` position — not a position in the filtered list. A
    /// regression here would silently hand a set another set's biome list.
    #[test]
    fn yielded_index_addresses_structure_set_all() {
        for dimension in [
            Dimension::OVERWORLD,
            Dimension::THE_NETHER,
            Dimension::THE_END,
        ] {
            let generator = generator_for(dimension);
            for (index, set) in generator.possible_structure_sets() {
                assert!(
                    std::ptr::eq(set, &StructureSet::ALL[index]),
                    "yielded index {index} does not address the yielded set"
                );
                // The biome list is keyed by the same index for every set, not
                // just the kept ones.
                assert_eq!(
                    generator.structure_allowed_biomes[&index],
                    crate::generation::proto_chunk::ProtoChunk::get_allowed_biomes(set)
                );
            }
            assert_eq!(
                generator.structure_allowed_biomes.len(),
                StructureSet::ALL.len(),
                "the biome map must still cover every set"
            );
        }
    }
}
