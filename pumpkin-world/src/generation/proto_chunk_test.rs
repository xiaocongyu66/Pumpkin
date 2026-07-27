#[cfg(test)]
mod test {
    use crate::biome::BiomeSupplier;
    use crate::chunk_system::chunk_state::StagedChunkEnum;
    use crate::generation::{
        biome::get_biome_blend, generator::WorldGenerator, get_world_gen, proto_chunk::ProtoChunk,
    };
    use pumpkin_data::{chunk::Biome, dimension::Dimension};
    use pumpkin_util::world_seed::Seed;

    #[test]
    fn no_blend_no_beard_0_0() {
        let seed = Seed(0);
        let world_gen = get_world_gen(seed, Dimension::OVERWORLD, false, Vec::new(), String::new());
        let mut chunk = ProtoChunk::new(0, 0, &world_gen);
        let WorldGenerator::Noise(generator) = &*world_gen else {
            unreachable!()
        };

        chunk.step_to_biomes(generator);
        chunk.stage = StagedChunkEnum::StructureReferences;
        chunk.step_to_noise(generator);

        let mut non_air_count = 0;
        for block in &chunk.flat_block_map {
            if !block.to_state().id.to_block().name.eq("air") {
                non_air_count += 1;
            }
        }
        assert!(
            non_air_count > 0,
            "Chunk should generate non-air noise blocks"
        );
    }

    #[test]
    fn no_blend_no_beard_7_4() {
        let seed = Seed(0);
        let world_gen = get_world_gen(seed, Dimension::OVERWORLD, false, Vec::new(), String::new());
        let mut chunk = ProtoChunk::new(7, 4, &world_gen);
        let WorldGenerator::Noise(generator) = &*world_gen else {
            unreachable!()
        };

        chunk.step_to_biomes(generator);
        chunk.stage = StagedChunkEnum::StructureReferences;
        chunk.step_to_noise(generator);

        let mut non_air_count = 0;
        for block in &chunk.flat_block_map {
            if !block.to_state().id.to_block().name.eq("air") {
                non_air_count += 1;
            }
        }
        assert!(
            non_air_count > 0,
            "Chunk should generate non-air noise blocks"
        );
    }

    #[test]
    fn no_blend_no_beard_surface_0_0() {
        let seed = Seed(0);
        let world_gen = get_world_gen(seed, Dimension::OVERWORLD, false, Vec::new(), String::new());
        let mut chunk = ProtoChunk::new(0, 0, &world_gen);
        let WorldGenerator::Noise(generator) = &*world_gen else {
            unreachable!()
        };

        chunk.step_to_biomes(generator);
        chunk.stage = StagedChunkEnum::StructureReferences;
        chunk.step_to_noise(generator);
        chunk.step_to_surface(generator);

        let bottom_block = chunk.get_block_state_raw(0, 0, 0);
        assert_eq!(
            bottom_block.to_state().id.to_block().name,
            "bedrock",
            "Bottom of the world must be bedrock"
        );

        let mut has_deepslate_or_stone = false;
        for y in 10..100 {
            let block = chunk.get_block_state_raw(8, y, 8);
            let name = block.to_state().id.to_block().name;
            if name.contains("deepslate") || name.eq("stone") {
                has_deepslate_or_stone = true;
                break;
            }
        }
        assert!(
            has_deepslate_or_stone,
            "Middle of the world must contain deepslate or stone"
        );

        let mut has_surface_blocks = false;
        for y in 100..384 {
            let block = chunk.get_block_state_raw(8, y, 8);
            let name = block.to_state().id.to_block().name;
            if name.eq("grass_block") || name.eq("dirt") || name.eq("sand") || name.eq("water") {
                has_surface_blocks = true;
                break;
            }
        }
        assert!(
            has_surface_blocks,
            "Top of the world must contain surface blocks (grass/dirt/sand/water)"
        );
    }

    #[test]
    fn surface_biome_resolver_uses_global_neighbor_quart() {
        let world_gen = get_world_gen(
            Seed(0),
            Dimension::OVERWORLD,
            false,
            Vec::new(),
            String::new(),
        );
        let WorldGenerator::Noise(generator) = &*world_gen else {
            unreachable!()
        };

        // `BiomeManager#getBiome` can select X=-1 for this column. The local
        // chunk palette has no such quart; its old clamp would have returned the
        // quart at X=0 instead of resolving the global source at X=-1.
        let (x, y, z) = (0, 64, 5);
        let selected_quart = get_biome_blend(
            generator.dimension.min_y as i8,
            generator.dimension.height as u16,
            generator.biome_mixer_seed,
            x,
            y,
            z,
        );
        assert_eq!(selected_quart.x, -1);

        let mut sampler = generator.terrain_gen_biome_sampler(0, 0);
        let expected = generator
            .terrain_gen_biome_at_block(x, y, z, &mut sampler)
            .id;

        let local_fallback = if expected == Biome::BADLANDS.id {
            Biome::PLAINS.id
        } else {
            Biome::BADLANDS.id
        };
        let mut local_chunk = ProtoChunk::new(0, 0, &world_gen);
        local_chunk.flat_biome_map.fill(local_fallback);
        assert_eq!(
            local_chunk.get_terrain_gen_biome_id(x, y, z),
            local_fallback
        );
        assert_ne!(
            expected, local_fallback,
            "the global resolver must not substitute the current chunk's palette"
        );

        // Independently sample the selected quart to prove the resolver did not
        // substitute the current chunk's palette at the boundary.
        let expected_quart_biome = crate::biome::MultiNoiseBiomeSupplier::OVERWORLD
            .biome(
                selected_quart.x,
                selected_quart.y,
                selected_quart.z,
                &mut sampler,
            )
            .id;
        assert_eq!(expected, expected_quart_biome);
    }

    #[test]
    fn nether_proto_chunk_uses_physical_height() {
        let world_gen = get_world_gen(
            Seed(0),
            Dimension::THE_NETHER,
            false,
            Vec::new(),
            String::new(),
        );
        let mut chunk = ProtoChunk::new(0, 0, &world_gen);
        let WorldGenerator::Noise(generator) = &*world_gen else {
            unreachable!()
        };

        assert_eq!(chunk.height(), Dimension::THE_NETHER.height as u16);
        assert_eq!(
            chunk.flat_block_map.len(),
            16 * 16 * Dimension::THE_NETHER.height as usize
        );

        // Biome population visits the complete physical range, including y=255.
        chunk.step_to_biomes(generator);
        chunk.set_block_state(0, 255, 0, pumpkin_data::Block::NETHERRACK.default_state);
        assert_eq!(
            chunk.get_block_state_raw(0, 255, 0),
            pumpkin_data::Block::NETHERRACK.default_state.id
        );
    }
}
