use pumpkin_data::chunk::Biome;

use crate::{
    biome::BiomeSupplier,
    generation::{
        biome_coords, noise::router::multi_noise_sampler::MultiNoiseSampler, section_coords,
    },
};

pub struct TheEndBiomeSupplier;

impl TheEndBiomeSupplier {
    const CENTER_BIOME: Biome = Biome::THE_END;
    const HIGHLANDS_BIOME: Biome = Biome::END_HIGHLANDS;
    const MIDLANDS_BIOME: Biome = Biome::END_MIDLANDS;
    const SMALL_ISLANDS_BIOME: Biome = Biome::SMALL_END_ISLANDS;
    const BARRENS_BIOME: Biome = Biome::END_BARRENS;

    /// Every biome this supplier can return, i.e. vanilla's
    /// `TheEndBiomeSource.collectPossibleBiomes`
    /// (`/root/Vanilla/src/net/minecraft/world/level/biome/TheEndBiomeSource.java:47-50`),
    /// which feeds `BiomeSource.possibleBiomes()`.
    pub const POSSIBLE_BIOMES: [u16; 5] = [
        Self::CENTER_BIOME.id as u16,
        Self::HIGHLANDS_BIOME.id as u16,
        Self::MIDLANDS_BIOME.id as u16,
        Self::SMALL_ISLANDS_BIOME.id as u16,
        Self::BARRENS_BIOME.id as u16,
    ];
}

impl BiomeSupplier for TheEndBiomeSupplier {
    fn biome(&self, x: i32, y: i32, z: i32, noise: &mut MultiNoiseSampler<'_>) -> &'static Biome {
        let x = biome_coords::to_block(x);
        let y = biome_coords::to_block(y);
        let z = biome_coords::to_block(z);
        let section_x = section_coords::block_to_section(x);
        let section_z = section_coords::block_to_section(z);
        if section_x * section_x + section_z * section_z <= 4096 {
            return &Self::CENTER_BIOME;
        }
        let x = (section_x * 2 + 1) * 8;
        let z = (section_z * 2 + 1) * 8;
        let noise = noise.sample_erosion(x, y, z);
        if noise > 0.25 {
            return &Self::HIGHLANDS_BIOME;
        }
        if noise >= -0.0625 {
            return &Self::MIDLANDS_BIOME;
        }
        if noise < -0.21875 {
            return &Self::SMALL_ISLANDS_BIOME;
        }

        &Self::BARRENS_BIOME
    }
}
