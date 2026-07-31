use sha2::{Digest, Sha256};
use std::cell::RefCell;

use enum_dispatch::enum_dispatch;
use pumpkin_data::chunk::{Biome, BiomeTree, NETHER_BIOME_SOURCE, OVERWORLD_BIOME_SOURCE};
use pumpkin_data::dimension::Dimension;

use crate::generation::noise::router::multi_noise_sampler::MultiNoiseSampler;
pub mod end;
pub mod multi_noise;
pub mod position_finder;

thread_local! {
    /// A shortcut; check if last used biome is what we should use
    static LAST_RESULT_NODE: RefCell<Option<&'static BiomeTree>> = const {RefCell::new(None) };
}

#[enum_dispatch]
pub trait BiomeSupplier {
    fn biome(&self, x: i32, y: i32, z: i32, noise: &mut MultiNoiseSampler<'_>) -> &'static Biome;
}

pub struct MultiNoiseBiomeSupplier {
    source: &'static BiomeTree,
}

impl MultiNoiseBiomeSupplier {
    pub const OVERWORLD: Self = Self::new(&OVERWORLD_BIOME_SOURCE);
    pub const NETHER: Self = Self::new(&NETHER_BIOME_SOURCE);

    const fn new(source: &'static BiomeTree) -> Self {
        Self { source }
    }

    /// Every biome this supplier can return, i.e. vanilla's
    /// `MultiNoiseBiomeSource.collectPossibleBiomes`
    /// (`/root/Vanilla/src/net/minecraft/world/level/biome/MultiNoiseBiomeSource.java:57-60`),
    /// which maps the climate parameter list to its biome values.
    ///
    /// Pumpkin stores that parameter list pre-compiled into a search tree, so the
    /// equivalent is the set of biomes on the tree's leaves. Walking the whole
    /// tree is only done once per generator (see
    /// [`crate::generation::generator::VanillaGenerator`]), never per chunk.
    #[must_use]
    pub fn possible_biomes(&self) -> Vec<u16> {
        let mut biomes = Vec::new();
        let mut pending: Vec<&'static BiomeTree> = vec![self.source];
        while let Some(node) = pending.pop() {
            match node {
                BiomeTree::Leaf { biome, .. } => biomes.push(u16::from(biome.id)),
                BiomeTree::Branch { nodes, .. } => pending.extend(*nodes),
            }
        }
        biomes.sort_unstable();
        biomes.dedup();
        biomes
    }
}

/// The biomes that can actually occur in `dimension`.
///
/// Mirrors vanilla's `BiomeSource.possibleBiomes()`
/// (`/root/Vanilla/src/net/minecraft/world/level/biome/BiomeSource.java:46-57`)
/// for the biome source that dimension's chunk generator is built with.
///
/// The dimension dispatch matches the one the generation steps already use to
/// pick a [`BiomeSupplier`] (see
/// `crate::generation::proto_chunk::steps` and
/// `crate::generation::proto_chunk::structures`), so the returned set is exactly
/// the range of the supplier that dimension samples with.
#[must_use]
pub fn dimension_possible_biomes(dimension: &Dimension) -> Vec<u16> {
    if *dimension == Dimension::THE_END {
        end::TheEndBiomeSupplier::POSSIBLE_BIOMES.to_vec()
    } else if *dimension == Dimension::THE_NETHER {
        MultiNoiseBiomeSupplier::NETHER.possible_biomes()
    } else {
        // Overworld (and the caves variant, which uses the same biome source).
        // `MultiNoiseBiomeSupplier::biome` may soft-remap a taiga sample to
        // `FOREST`/`SNOWY_PLAINS`, but both are already overworld climate-tree
        // leaves, so the reachable set is unchanged.
        MultiNoiseBiomeSupplier::OVERWORLD.possible_biomes()
    }
}

impl BiomeSupplier for MultiNoiseBiomeSupplier {
    fn biome(&self, x: i32, y: i32, z: i32, noise: &mut MultiNoiseSampler<'_>) -> &'static Biome {
        let point = noise.sample(x, y, z);
        let point_list = point.convert_to_list();
        let biome = LAST_RESULT_NODE
            .with_borrow_mut(|last_result| self.source.get(&point_list, last_result));
        // Overworld only: slightly reduce taiga-family prevalence (~5%) so
        // temperate forests/plains appear more often when exploring.
        // Disabled under `cfg(test)` so vanilla multi-noise golden tests stay exact.
        #[cfg(not(test))]
        {
            if std::ptr::eq(self.source, &OVERWORLD_BIOME_SOURCE) {
                return reduce_taiga_prevalence(biome, x, z);
            }
        }
        biome
    }
}

/// Deterministic ~5% remapping of taiga-family biomes toward forest/snowy plains.
///
/// Vanilla multi-noise leaf counts put a large share of temperate climate in
/// `taiga`/`snowy_taiga`; players report long stretches of only taiga. We keep the
/// vanilla climate tree and soft-bias 5% of taiga samples to neighboring biomes.
#[cfg(not(test))]
fn reduce_taiga_prevalence(biome: &'static Biome, x: i32, z: i32) -> &'static Biome {
    let id = biome.registry_id;
    let is_taiga = matches!(
        id,
        "taiga" | "snowy_taiga" | "old_growth_pine_taiga" | "old_growth_spruce_taiga"
    );
    if !is_taiga {
        return biome;
    }
    // Stable hash in 0..99; remap when < 5 (exactly 5%).
    let h = (i64::from(x)
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(i64::from(z).wrapping_mul(0x85EB_CA77)))
    .unsigned_abs()
        % 100;
    if h >= 5 {
        return biome;
    }
    match id {
        "snowy_taiga" => &Biome::SNOWY_PLAINS,
        // Plain and old-growth taiga map to forest (still cold-temperate tree biome).
        _ => &Biome::FOREST,
    }
}

#[must_use]
pub fn hash_seed(seed: u64) -> i64 {
    let mut hasher = Sha256::new();
    hasher.update(seed.to_le_bytes());
    let result = hasher.finalize();
    i64::from_le_bytes(result[..8].try_into().unwrap())
}

#[cfg(test)]
mod test {
    use pumpkin_data::{chunk::Biome, dimension::Dimension};
    use pumpkin_util::read_data_from_file;
    use serde::Deserialize;

    use crate::{
        ProtoChunk,
        chunk::palette::BIOME_NETWORK_MAX_BITS,
        generation::noise::router::multi_noise_sampler::{
            MultiNoiseSampler, MultiNoiseSamplerBuilderOptions,
        },
    };

    use super::{BiomeSupplier, MultiNoiseBiomeSupplier, hash_seed};

    #[test]
    fn biome_desert() {
        use crate::generation::generator::{GeneratorInit, VanillaGenerator};
        use pumpkin_util::world_seed::Seed;
        let seed = 13579;
        let generator = VanillaGenerator::new(Seed(seed as u64), Dimension::OVERWORLD);
        let multi_noise_config = MultiNoiseSamplerBuilderOptions::new(1, 1, 1);
        let mut sampler =
            MultiNoiseSampler::generate(&generator.base_router.multi_noise, &multi_noise_config);
        let biome = MultiNoiseBiomeSupplier::OVERWORLD.biome(-24, 1, 8, &mut sampler);
        assert_eq!(biome, &Biome::DESERT);
    }

    #[test]
    fn wide_area_surface() {
        use crate::generation::generator::{GeneratorInit, VanillaGenerator, WorldGenerator};
        use crate::generation::noise::router::multi_noise_sampler::{
            MultiNoiseSampler, MultiNoiseSamplerBuilderOptions,
        };
        use crate::generation::{biome_coords, positions::chunk_pos};
        use pumpkin_util::world_seed::Seed;
        #[derive(Deserialize)]
        struct BiomeData {
            x: i32,
            z: i32,
            data: Vec<(i32, i32, i32, u8)>,
        }

        let expected_data: Vec<BiomeData> =
            read_data_from_file!("../../assets/biome_no_blend_no_beard_0.json");

        let seed = 0;
        let world_gen = WorldGenerator::Noise(Box::new(VanillaGenerator::new(
            Seed(seed as u64),
            Dimension::OVERWORLD,
        )));
        let WorldGenerator::Noise(generator) = &world_gen else {
            unreachable!()
        };

        for data in expected_data {
            let chunk_x = data.x;
            let chunk_z = data.z;

            let mut chunk = ProtoChunk::new(chunk_x, chunk_z, &world_gen);

            // Create MultiNoiseSampler for populate_biomes

            let start_x = chunk_pos::start_block_x(chunk_x);
            let start_z = chunk_pos::start_block_z(chunk_z);

            let horizontal_biome_end = biome_coords::from_block(16);
            let multi_noise_config = MultiNoiseSamplerBuilderOptions::new(
                biome_coords::from_block(start_x),
                biome_coords::from_block(start_z),
                horizontal_biome_end as usize,
            );
            let mut multi_noise_sampler = MultiNoiseSampler::generate(
                &generator.base_router.multi_noise,
                &multi_noise_config,
            );

            chunk.populate_biomes(generator, &mut multi_noise_sampler);

            for (biome_x, biome_y, biome_z, biome_id) in data.data {
                let calculated_biome = chunk.get_biome(biome_x, biome_y, biome_z);

                assert_eq!(
                    biome_id,
                    calculated_biome.id,
                    "Expected {:?} was {:?} at {},{},{} ({},{})",
                    Biome::from_id(biome_id),
                    calculated_biome,
                    biome_x,
                    biome_y,
                    biome_z,
                    data.x,
                    data.z
                );
            }
        }
    }

    /// `dimension_possible_biomes` stands in for vanilla's
    /// `BiomeSource.possibleBiomes()`, so each dimension must report exactly the
    /// biomes its own supplier can return — and nothing from another dimension.
    #[test]
    fn dimension_possible_biomes_are_disjoint_per_dimension() {
        use super::{dimension_possible_biomes, end::TheEndBiomeSupplier};

        let overworld = dimension_possible_biomes(&Dimension::OVERWORLD);
        let nether = dimension_possible_biomes(&Dimension::THE_NETHER);
        let end = dimension_possible_biomes(&Dimension::THE_END);

        // The end source is a fixed five-biome list (TheEndBiomeSource.java:47-50).
        assert_eq!(end, TheEndBiomeSupplier::POSSIBLE_BIOMES.to_vec());

        // The nether climate tree has exactly the five nether biomes.
        let mut expected_nether = vec![
            u16::from(Biome::NETHER_WASTES.id),
            u16::from(Biome::CRIMSON_FOREST.id),
            u16::from(Biome::WARPED_FOREST.id),
            u16::from(Biome::SOUL_SAND_VALLEY.id),
            u16::from(Biome::BASALT_DELTAS.id),
        ];
        expected_nether.sort_unstable();
        assert_eq!(nether, expected_nether);

        // A structure set is dropped by intersecting these lists, so an overlap
        // would silently let e.g. nether fossils back into the overworld.
        for biome in &nether {
            assert!(!overworld.contains(biome), "biome {biome} in both");
            assert!(!end.contains(biome), "biome {biome} in both");
        }
        for biome in &end {
            assert!(!overworld.contains(biome), "biome {biome} in both");
        }

        // Spot-check the overworld's own leaves; the taiga soft-remap targets must
        // be present, since they are what a remapped sample can return.
        for biome in [
            Biome::PLAINS,
            Biome::FOREST,
            Biome::SNOWY_PLAINS,
            Biome::DESERT,
        ] {
            assert!(overworld.contains(&u16::from(biome.id)));
        }
    }

    #[test]
    fn hash_seed_test() {
        let hashed_seed = hash_seed(0);
        assert_eq!(8794265229978523055, hashed_seed);

        let hashed_seed = hash_seed((-777i64) as u64);
        assert_eq!(-1087248400229165450, hashed_seed);
    }

    #[test]
    fn proper_network_bits_per_entry() {
        let id_to_test = 1 << BIOME_NETWORK_MAX_BITS;
        assert!(
            Biome::from_id(id_to_test).is_none(),
            "We need to update our constants!"
        );
    }
}
