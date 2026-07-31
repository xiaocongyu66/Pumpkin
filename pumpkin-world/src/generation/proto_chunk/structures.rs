use std::sync::Arc;

use pumpkin_data::chunk::Biome;
use pumpkin_data::dimension::Dimension;
use pumpkin_data::structures::{Structure, StructurePlacementType, StructureSet, WeightedEntry};
use pumpkin_data::tag::{RegistryKey, get_tag_ids};
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::random::{
    RandomGenerator, RandomImpl, get_decorator_seed, xoroshiro128::Xoroshiro,
};

use crate::biome::{BiomeSupplier, MultiNoiseBiomeSupplier, end::TheEndBiomeSupplier};
use crate::chunk_system::StagedChunkEnum;
use crate::generation::diagnostics;
use crate::generation::structure::lazily_generate_structure;
use crate::generation::structure::placement::should_generate_structure;
use crate::generation::structure::structures::{
    StructureGeneratorContext, StructureInstance, StructurePosition, create_chunk_random,
};
use crate::generation::structure::try_generate_structure;
use crate::generation::{
    GlobalRandomConfig,
    blender::{Blender, BlenderImpl},
    feature::placed_features::PLACED_FEATURES,
    noise::router::multi_noise_sampler::{MultiNoiseSampler, MultiNoiseSamplerBuilderOptions},
    positions::chunk_pos,
};
use crate::world::WorldPortalExt;

use super::{ActiveSupplier, GenerationCache, ProtoChunk};

/// Selects a structure-set entry using Vanilla's `ChunkGenerator.createStructures`
/// weighted fallback order (`/root/Vanilla/src/net/minecraft/world/level/chunk/
/// ChunkGenerator.java:401-427`).
///
/// Multi-entry sets use `WorldgenRandom(new LegacyRandomSource(0))` seeded by
/// `setLargeFeatureSeed(seed, chunk_x, chunk_z)`
/// (`ChunkGenerator.java:407-408`,
/// `/root/Vanilla/src/net/minecraft/world/level/levelgen/WorldgenRandom.java:69-75`),
/// which is the same Legacy RNG returned by `create_chunk_random`. That single
/// instance is reused for every retry: each failed candidate costs exactly one
/// `nextInt(total)` draw, is removed, and its weight is subtracted so the next
/// draw is bounded by the shrunk total (`ChunkGenerator.java:415`, `:425-426`).
/// Single-entry sets bypass this selection RNG entirely, as Vanilla does
/// (`ChunkGenerator.java:401-404`).
fn try_select_structure_set_entry<'a, T>(
    entries: &'a [WeightedEntry],
    seed: i64,
    chunk_x: i32,
    chunk_z: i32,
    mut try_entry: impl FnMut(&'a WeightedEntry) -> Option<T>,
) -> Option<(&'a WeightedEntry, T)> {
    match entries {
        [] => None,
        [entry] => try_entry(entry).map(|result| (entry, result)),
        _ => {
            let mut candidates: Vec<&WeightedEntry> = entries.iter().collect();
            let mut random = create_chunk_random(seed, chunk_x, chunk_z);
            let mut total_weight = candidates.iter().fold(0u32, |total, entry| {
                total
                    .checked_add(entry.weight)
                    .expect("structure-set total weight fits in u32")
            });

            // `while (!options.isEmpty())` (`ChunkGenerator.java:413-427`). The RNG
            // advance on the retry path is the subtle part, so it is mirrored
            // literally:
            // * `ChunkGenerator.java:415` draws `random.nextInt(total)` once per
            //   loop iteration, i.e. exactly one bounded draw per candidate
            //   attempt - a failed candidate consumes one draw, no more, no less.
            // * The draw uses the *same* `WorldgenRandom` built once before the
            //   loop (`ChunkGenerator.java:407-408`); vanilla never re-seeds or
            //   forks it between attempts, so the failure path must not call
            //   `create_chunk_random` again.
            // * `ChunkGenerator.java:425-426` removes the failed candidate and
            //   subtracts its weight, so the next draw is bounded by the shrunk
            //   total. Bound changes are what make the sequence diverge from a
            //   naive "reroll with the original total" implementation.
            // Note `next_bounded_i32` itself may consume more than one LCG step
            // for non-power-of-two bounds; that rejection loop is identical in
            // `BitRandomSource.nextInt` and `LegacyRand::next_bounded_i32`.
            while !candidates.is_empty() {
                let mut choice = random.next_bounded_i32(
                    i32::try_from(total_weight).expect("structure-set total weight fits in i32"),
                );
                // `ChunkGenerator.java:416-421`: walk the remaining candidates in
                // order, subtracting each weight, and take the first one that
                // drives the running choice negative. The scan reads no RNG, so
                // the fallback below cannot change the draw count: `choice` is
                // always `< total_weight`, hence some candidate always drives it
                // negative and `selected_index = 0` is unreachable (vanilla's
                // equivalent `options.get(index)` would throw there).
                let mut selected_index = 0;

                for (index, entry) in candidates.iter().enumerate() {
                    choice -= i32::try_from(entry.weight)
                        .expect("structure-set entry weight fits in i32");
                    if choice < 0 {
                        selected_index = index;
                        break;
                    }
                }

                let selected = candidates[selected_index];
                // `ChunkGenerator.java:422-424`: the first candidate that places
                // wins and the remaining candidates are never offered.
                if let Some(result) = try_entry(selected) {
                    return Some((selected, result));
                }

                // `ChunkGenerator.java:425-426`, in that order: drop the failed
                // candidate, then shrink the total by its weight.
                total_weight -= candidates.remove(selected_index).weight;
            }

            None
        }
    }
}

impl ProtoChunk {
    pub fn generate_features_and_structure<T: GenerationCache>(
        cache: &mut T,
        block_registry: &dyn WorldPortalExt,
        random_config: &GlobalRandomConfig,
    ) {
        let (center_x, center_z, min_y, height, biomes_in_chunk) = {
            let chunk = cache.get_center_chunk();
            let mut unique_biomes = Vec::with_capacity(4);
            for &biome_id in &chunk.flat_biome_map {
                if !unique_biomes.contains(&biome_id) {
                    unique_biomes.push(biome_id);
                }
            }
            (
                chunk.x,
                chunk.z,
                chunk.bottom_y() as i32,
                chunk.height() as i32,
                unique_biomes,
            )
        };

        let start_block_x = chunk_pos::start_block_x(center_x);
        let start_block_z = chunk_pos::start_block_z(center_z);
        let origin_pos = BlockPos::new(start_block_x, min_y, start_block_z);

        let population_seed =
            Xoroshiro::get_population_seed(random_config.seed, start_block_x, start_block_z);

        let diagnose = diagnostics::enabled();

        for step in 0..11 {
            // `Instant::now` is only taken in development mode; the release path
            // keeps the plain call.
            let step_started = diagnose.then(std::time::Instant::now);

            let collectors = Self::generate_structure_step(
                cache,
                block_registry,
                step,
                population_seed,
                random_config.seed as i64,
            );

            let mut features_to_run = Vec::new();
            for biome_id in &biomes_in_chunk {
                if let Some(biome) = Biome::from_id(*biome_id)
                    && let Some(features_at_step) = biome.features.get(step)
                {
                    for &feature_id in *features_at_step {
                        features_to_run.push(feature_id);
                    }
                }
            }

            features_to_run.sort_unstable();
            features_to_run.dedup();

            for (p, feature_enum) in features_to_run.into_iter().enumerate() {
                if let Some(feature) = PLACED_FEATURES.get(&feature_enum) {
                    let decorator_seed = get_decorator_seed(population_seed, p as u64, step as u64);
                    let mut random =
                        RandomGenerator::Xoroshiro(Xoroshiro::from_seed(decorator_seed));

                    feature.generate(
                        cache,
                        block_registry,
                        min_y as i8,
                        height as u16,
                        feature_enum,
                        &mut random,
                        origin_pos,
                    );
                }
            }

            if let Some(started) = step_started {
                diagnostics::feature_step_slow(
                    center_x,
                    center_z,
                    step,
                    collectors,
                    started.elapsed().as_millis(),
                );
            }
        }

        cache.get_center_chunk_mut().stage = StagedChunkEnum::Features;
    }

    /// Runs the structure piece collectors scheduled for `step` and returns how
    /// many of them ran (used only by development diagnostics).
    fn generate_structure_step<T: GenerationCache>(
        cache: &mut T,
        block_registry: &dyn WorldPortalExt,
        step: usize,
        population_seed: u64,
        world_seed: i64,
    ) -> usize {
        let mut tasks = Vec::new();
        {
            let center_chunk = cache.get_center_chunk();
            let center_x = center_chunk.x;
            let center_z = center_chunk.z;

            for (id, instance) in &center_chunk.structure_starts {
                let s = Structure::get(id);
                if s.step.ordinal() != step {
                    continue;
                }

                match instance {
                    StructureInstance::Start(pos) => tasks.push(pos.collector.clone()),
                    StructureInstance::Reference(collector) => {
                        let collector_arc = collector.clone();
                        if !tasks.iter().any(|t| Arc::ptr_eq(t, &collector_arc)) {
                            tasks.push(collector_arc);
                        }
                    }
                }
            }

            let radius = 8;
            for dx in -radius..=radius {
                for dz in -radius..=radius {
                    if dx == 0 && dz == 0 {
                        continue;
                    }

                    let neighbor_x = center_x + dx;
                    let neighbor_z = center_z + dz;

                    if let Some(neighbor) = cache.try_get_proto_chunk(neighbor_x, neighbor_z) {
                        for (id, instance) in &neighbor.structure_starts {
                            let s = Structure::get(id);
                            if s.step.ordinal() != step {
                                continue;
                            }

                            match instance {
                                StructureInstance::Start(pos) => {
                                    let start_x = chunk_pos::start_block_x(center_x);
                                    let start_z = chunk_pos::start_block_z(center_z);
                                    let end_x = start_x + 15;
                                    let end_z = start_z + 15;

                                    if pos
                                        .get_bounding_box()
                                        .intersects_raw_xz(start_x, start_z, end_x, end_z)
                                    {
                                        let collector_arc = pos.collector.clone();
                                        if !tasks.iter().any(|t| Arc::ptr_eq(t, &collector_arc)) {
                                            tasks.push(collector_arc);
                                        }
                                    }
                                }
                                StructureInstance::Reference(collector) => {
                                    let collector_arc = collector.clone();
                                    if !tasks.iter().any(|t| Arc::ptr_eq(t, &collector_arc)) {
                                        tasks.push(collector_arc);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let decorator_seed = get_decorator_seed(population_seed, 0, step as u64);
        let mut random = RandomGenerator::Xoroshiro(Xoroshiro::from_seed(decorator_seed));

        let collectors = tasks.len();
        let chunk = cache.get_center_chunk_mut();
        for collector_arc in tasks {
            let mut collector = collector_arc.lock().unwrap();
            collector.generate_in_chunk(chunk, block_registry, &mut random, world_seed);
        }
        collectors
    }

    #[must_use]
    pub fn get_allowed_biomes(set: &StructureSet) -> Vec<u16> {
        let mut allowed_biomes = Vec::new();
        for entry in set.structures {
            let structure = Structure::get(&entry.structure);
            if let Some(biomes) = get_tag_ids(
                RegistryKey::WorldgenBiome,
                structure
                    .biomes
                    .strip_prefix('#')
                    .unwrap_or(structure.biomes),
            ) {
                allowed_biomes.extend_from_slice(biomes);
            }
        }
        allowed_biomes
    }

    pub fn set_structure_starts(
        &mut self,
        generator: &crate::generation::generator::VanillaGenerator,
    ) {
        debug_assert_eq!(self.stage, StagedChunkEnum::Biomes);
        let random_config = &generator.random_config;
        let settings = generator.settings;
        let global_cache = &generator.global_structure_cache;
        let calculator = &generator.structure_calculator;

        let seed = random_config.seed;
        let mut height_sampler = crate::generation::noise::router::surface_height_sampler::SurfaceHeightEstimateSampler::generate(
            &generator.base_router.surface_estimator,
            &crate::generation::noise::router::surface_height_sampler::SurfaceHeightSamplerBuilderOptions::new(
                crate::generation::biome_coords::from_block(chunk_pos::start_block_x(self.x)),
                crate::generation::biome_coords::from_block(chunk_pos::start_block_z(self.z)),
                4,
                settings.shape.min_y as i32,
                settings.shape.height as i32,
                (settings.shape.height / settings.shape.vertical_cell_block_count() as u16) as usize,
            ),
        );

        let diagnose = diagnostics::enabled();

        // `ChunkGenerator.createStructures` iterates
        // `state.possibleStructureSets()` (`ChunkGenerator.java:605`), i.e. only the
        // sets whose structures can occur in this dimension's biomes. `set_index`
        // stays the `StructureSet::ALL` index the biome lists are keyed by.
        for (set_index, set) in generator.possible_structure_sets() {
            let allowed_biomes = &generator.structure_allowed_biomes[&set_index];

            let verdict = should_generate_structure(
                &set.placement,
                calculator,
                self.x,
                self.z,
                global_cache,
                self,
                allowed_biomes,
            );
            if !verdict.accepted() {
                if diagnose {
                    diagnostics::structure_placement_rejected(
                        set.structures,
                        self.x,
                        self.z,
                        verdict,
                    );
                }
                continue;
            }

            let selected = try_select_structure_set_entry(
                set.structures,
                seed as i64,
                self.x,
                self.z,
                |entry| {
                    self.try_generate_structure_start(
                        settings.sea_level,
                        entry,
                        random_config,
                        &mut height_sampler,
                    )
                },
            );

            if let Some((entry, position)) = selected {
                self.structure_starts
                    .insert(entry.structure, StructureInstance::Start(position));
            } else if diagnose && !set.structures.is_empty() {
                // The helper only returns `None` once every weighted candidate has
                // been offered and rejected, which is what the old
                // `candidates.is_empty()` check stood for.
                diagnostics::structure_set_exhausted(set.structures, self.x, self.z);
            }
        }
        self.stage = StagedChunkEnum::StructureStart;
    }

    fn try_generate_structure_start(
        &self,
        sea_level: i32,
        entry: &WeightedEntry,
        random_config: &GlobalRandomConfig,
        height_sampler: &mut crate::generation::noise::router::surface_height_sampler::SurfaceHeightEstimateSampler<'_>,
    ) -> Option<StructurePosition> {
        try_generate_structure(
            &entry.structure,
            Structure::get(&entry.structure),
            random_config.seed as i64,
            self,
            sea_level,
            Some(height_sampler),
        )
    }

    #[expect(clippy::too_many_lines)]
    pub fn set_structure_references(
        &mut self,
        generator: &crate::generation::generator::VanillaGenerator,
    ) {
        debug_assert_eq!(self.stage, StagedChunkEnum::StructureStart);
        let random_config = &generator.random_config;
        let settings = generator.settings;
        let dimension = &generator.dimension;
        let noise_router = &generator.base_router;
        let global_cache = &generator.global_structure_cache;

        let start_x = chunk_pos::start_block_x(self.x);
        let start_z = chunk_pos::start_block_z(self.z);
        let end_x = start_x + 15;
        let end_z = start_z + 15;

        let seed = random_config.seed as i64;

        let active_supplier = if *dimension == Dimension::THE_END {
            ActiveSupplier::End(TheEndBiomeSupplier)
        } else if *dimension == Dimension::THE_NETHER {
            ActiveSupplier::Nether(MultiNoiseBiomeSupplier::NETHER)
        } else {
            ActiveSupplier::Overworld(MultiNoiseBiomeSupplier::OVERWORLD)
        };

        let base_supplier: &dyn BiomeSupplier = match &active_supplier {
            ActiveSupplier::End(s) => s,
            ActiveSupplier::Nether(s) | ActiveSupplier::Overworld(s) => s,
        };
        let blender = Blender::empty();
        let biome_supplier = blender.get_biome_supplier(base_supplier);
        let multi_noise_config = MultiNoiseSamplerBuilderOptions::new(0, 0, 0);
        let mut multi_noise_sampler =
            MultiNoiseSampler::generate(&noise_router.multi_noise, &multi_noise_config);

        let mut height_sampler = crate::generation::noise::router::surface_height_sampler::SurfaceHeightEstimateSampler::generate(
            &noise_router.surface_estimator,
            &crate::generation::noise::router::surface_height_sampler::SurfaceHeightSamplerBuilderOptions::new(
                crate::generation::biome_coords::from_block(start_x),
                crate::generation::biome_coords::from_block(start_z),
                4,
                settings.shape.min_y as i32,
                settings.shape.height as i32,
                (settings.shape.height / settings.shape.vertical_cell_block_count() as u16) as usize,
            ),
        );

        let mut references = Vec::new();
        let chunk_min_y = self.bottom_y() as i32;
        let calculator = &generator.structure_calculator;
        let diagnose = diagnostics::enabled();

        // Same dimension pre-filter as the start pass: vanilla propagates
        // references from actual starts only, and a set that cannot start in this
        // dimension has none. `set_index` remains the `StructureSet::ALL` index.
        for (set_index, set) in generator.possible_structure_sets() {
            let set_allowed_biomes = &generator.structure_allowed_biomes[&set_index];
            let mut candidate_chunks = Vec::new();

            match &set.placement.placement_type {
                StructurePlacementType::RandomSpread(spread) => {
                    // Vanilla ChunkGenerator.createReferences (ChunkGenerator.java
                    // l.450-458) scans the actual starts of every chunk within 8
                    // chunks of this one. Cover every placement region that can
                    // contain a candidate chunk inside that window: for spacing-1
                    // sets (minecraft:mineshafts, spacing 1) this is the full
                    // 17x17 chunk neighborhood, where a region +-1 scan would
                    // only reach chunks +-1 away and truncate sprawling
                    // structures at that boundary.
                    let region_min_x = pumpkin_util::math::floor_div(self.x - 8, spread.spacing);
                    let region_max_x = pumpkin_util::math::floor_div(self.x + 8, spread.spacing);
                    let region_min_z = pumpkin_util::math::floor_div(self.z - 8, spread.spacing);
                    let region_max_z = pumpkin_util::math::floor_div(self.z + 8, spread.spacing);

                    for rx in region_min_x..=region_max_x {
                        for rz in region_min_z..=region_max_z {
                            candidate_chunks.push(
                                crate::generation::structure::placement::get_structure_chunk_in_region(
                                    spread,
                                    seed,
                                    rx,
                                    rz,
                                    set.placement.salt,
                                )
                            );
                        }
                    }
                }
                StructurePlacementType::ConcentricRings(rings) => {
                    let allowed_biomes = Self::get_allowed_biomes(set);
                    let strongholds = global_cache.get_or_calculate_strongholds(
                        seed,
                        rings,
                        self,
                        &allowed_biomes,
                    );
                    for &(cx, cz) in strongholds {
                        if (cx - self.x).abs() <= 8 && (cz - self.z).abs() <= 8 {
                            candidate_chunks.push((cx, cz));
                        }
                    }
                }
            }

            for (candidate_chunk_x, candidate_chunk_z) in candidate_chunks {
                if (candidate_chunk_x - self.x).abs() <= 8
                    && (candidate_chunk_z - self.z).abs() <= 8
                {
                    // Vanilla only ever creates a start where
                    // StructurePlacement.isStructureChunk passes (placement chunk
                    // AND frequency reduction, StructurePlacement.java l.77-83;
                    // gated in ChunkGenerator.createStructures l.398), and
                    // createReferences propagates only those actual starts. The
                    // same gate must apply to recomputed candidates here:
                    // minecraft:mineshafts has spacing 1 / frequency 0.004, so
                    // without it every biome-valid chunk becomes a phantom
                    // mineshaft start.
                    if !should_generate_structure(
                        &set.placement,
                        calculator,
                        candidate_chunk_x,
                        candidate_chunk_z,
                        global_cache,
                        self,
                        set_allowed_biomes,
                    )
                    .accepted()
                    {
                        continue;
                    }
                    if let Some((entry, start_data)) = try_select_structure_set_entry(
                        set.structures,
                        seed,
                        candidate_chunk_x,
                        candidate_chunk_z,
                        |entry| {
                            let structure = Structure::get(&entry.structure);

                            // A structure's placement depends only on its start chunk and the
                            // world seed, so cache it: otherwise every surrounding chunk whose
                            // references overlap it would re-run the (expensive) jigsaw
                            // expansion. `context` is only built on a cache miss.
                            let start_data = global_cache.get_or_compute_structure_start(
                                entry.structure,
                                candidate_chunk_x,
                                candidate_chunk_z,
                                || {
                                    let context = StructureGeneratorContext {
                                        seed,
                                        chunk_x: candidate_chunk_x,
                                        chunk_z: candidate_chunk_z,
                                        random: create_chunk_random(
                                            seed,
                                            candidate_chunk_x,
                                            candidate_chunk_z,
                                        ),
                                        sea_level: settings.sea_level,
                                        min_y: chunk_min_y,
                                        max_y: chunk_min_y + self.height() as i32 - 1,
                                        height_sampler: Some(&mut height_sampler),
                                        structure_key: Some(entry.structure),
                                    };
                                    lazily_generate_structure(
                                        &entry.structure,
                                        structure,
                                        context,
                                        &biome_supplier,
                                        &mut multi_noise_sampler,
                                    )
                                },
                            );

                            if start_data.is_none() && diagnose {
                                // The placement gate accepted this chunk, yet no start
                                // came out of the generator: any piece vanilla would
                                // have placed here is missing. This is the signature of
                                // a truncated structure (broken mineshaft, half a
                                // village), so it is worth a (sampled) line.
                                diagnostics::structure_reference_missing_start(
                                    entry.structure,
                                    candidate_chunk_x,
                                    candidate_chunk_z,
                                    self.x,
                                    self.z,
                                );
                            }

                            start_data
                        },
                    ) && start_data
                        .get_bounding_box()
                        .intersects_raw_xz(start_x, start_z, end_x, end_z)
                    {
                        if diagnose {
                            diagnostics::structure_reference_attached(
                                entry.structure,
                                candidate_chunk_x,
                                candidate_chunk_z,
                                self.x,
                                self.z,
                            );
                        }
                        references.push((entry.structure, start_data.collector.clone()));
                    }
                }
            }
        }

        for (key, pos) in references {
            self.structure_starts
                .entry(key)
                .or_insert_with(|| StructureInstance::Reference(pos));
        }

        self.stage = StagedChunkEnum::StructureReferences;
    }
}

#[cfg(test)]
mod tests {
    use pumpkin_data::structures::{StructureKeys, StructureSet, WeightedEntry};

    use super::try_select_structure_set_entry;

    #[test]
    fn multi_entry_selection_uses_vanilla_large_feature_seed() {
        let (entry, ()) = try_select_structure_set_entry(
            StructureSet::MINESHAFTS.structures,
            123_456_789,
            -37,
            84,
            |_| Some(()),
        )
        .expect("a mineshaft variant is selected");

        // Vanilla WorldgenRandom(LegacyRandomSource) seeded with
        // setLargeFeatureSeed(123456789, -37, 84) first returns nextInt(2) == 1
        // (`WorldgenRandom.java:69-75`, `ChunkGenerator.java:405-427`).
        assert_eq!(entry.structure, StructureKeys::MineshaftMesa);
    }

    #[test]
    fn multi_entry_selection_retries_without_replacement() {
        let (entry, ()) = try_select_structure_set_entry(
            StructureSet::MINESHAFTS.structures,
            123_456_789,
            -37,
            84,
            |entry| (entry.structure == StructureKeys::Mineshaft).then_some(()),
        )
        .expect("the fallback mineshaft variant succeeds");

        assert_eq!(entry.structure, StructureKeys::Mineshaft);
    }

    #[test]
    fn owner_and_reference_selection_match_for_multi_entry_set() {
        let choose = || {
            try_select_structure_set_entry(
                StructureSet::NETHER_COMPLEXES.structures,
                987_654_321,
                12,
                -15,
                |entry| (entry.structure == StructureKeys::BastionRemnant).then_some(()),
            )
            .map(|(entry, ())| entry.structure)
        };

        // The owner start and a reference recomputation both invoke the same helper
        // with the start chunk coordinates, so they cannot drift to static entry order.
        assert_eq!(choose(), Some(StructureKeys::BastionRemnant));
        assert_eq!(choose(), Some(StructureKeys::BastionRemnant));
    }

    /// Records the exact order in which candidates are offered, so a change in
    /// RNG behaviour (extra/missing draw, wrong seeding, wrong index scan) shows
    /// up as a sequence mismatch instead of a silent world-gen drift.
    fn offer_order(
        entries: &'static [WeightedEntry],
        seed: i64,
        chunk_x: i32,
        chunk_z: i32,
        accept: impl Fn(StructureKeys) -> bool,
    ) -> (Vec<StructureKeys>, Option<StructureKeys>) {
        let mut offered = Vec::new();
        let selected = try_select_structure_set_entry(entries, seed, chunk_x, chunk_z, |entry| {
            offered.push(entry.structure);
            accept(entry.structure).then_some(())
        })
        .map(|(entry, ())| entry.structure);

        (offered, selected)
    }

    #[test]
    fn seven_entry_set_exhausts_in_vanilla_order() {
        // Oracle: vanilla `ChunkGenerator.createStructures` (ChunkGenerator.java:405-427)
        // with `WorldgenRandom(new LegacyRandomSource(0))` +
        // `setLargeFeatureSeed(123456789, 0, 0)` (WorldgenRandom.java:69-75) draws
        // nextInt with the shrinking total weight: 1/7, 0/6, 3/5, 1/4, 0/3, 0/2, 0/1.
        let (offered, selected) = offer_order(
            StructureSet::RUINED_PORTALS.structures,
            123_456_789,
            0,
            0,
            |_| false,
        );

        assert_eq!(selected, None);
        assert_eq!(
            offered,
            vec![
                StructureKeys::RuinedPortalDesert,
                StructureKeys::RuinedPortal,
                StructureKeys::RuinedPortalOcean,
                StructureKeys::RuinedPortalSwamp,
                StructureKeys::RuinedPortalJungle,
                StructureKeys::RuinedPortalMountain,
                StructureKeys::RuinedPortalNether,
            ]
        );
    }

    #[test]
    fn failed_placements_walk_the_same_sequence_as_vanilla() {
        // Same fixture as above, but the last candidate is the one that places:
        // every earlier failure must consume exactly one draw and remove its weight.
        let (offered, selected) = offer_order(
            StructureSet::RUINED_PORTALS.structures,
            123_456_789,
            0,
            0,
            |key| key == StructureKeys::RuinedPortalNether,
        );

        assert_eq!(selected, Some(StructureKeys::RuinedPortalNether));
        assert_eq!(offered.len(), 7);
        assert_eq!(offered[0], StructureKeys::RuinedPortalDesert);
        assert_eq!(offered[6], StructureKeys::RuinedPortalNether);
    }

    #[test]
    fn weighted_retry_stops_at_the_first_success() {
        // minecraft:villages, seed 987654321, chunk (5, -9): vanilla offers
        // desert, plains, taiga (ChunkGenerator.java:413-427). Accepting taiga must
        // stop there, leaving snowy/savanna untouched.
        let (offered, selected) = offer_order(
            StructureSet::VILLAGES.structures,
            987_654_321,
            5,
            -9,
            |key| key == StructureKeys::VillageTaiga,
        );

        assert_eq!(selected, Some(StructureKeys::VillageTaiga));
        assert_eq!(
            offered,
            vec![
                StructureKeys::VillageDesert,
                StructureKeys::VillagePlains,
                StructureKeys::VillageTaiga,
            ]
        );
    }

    #[test]
    fn single_entry_set_offers_once_and_skips_the_selection_rng() {
        // Vanilla short-circuits `structures.size() == 1` before constructing the
        // WorldgenRandom (ChunkGenerator.java:401-404), so a single-entry set must
        // consume zero selection draws: the entry is offered exactly once, and the
        // outcome is identical for every seed / chunk coordinate.
        for (seed, chunk_x, chunk_z) in [(123_456_789, 7, 7), (-42, -1_000, 999), (0, 0, 0)] {
            let (offered, selected) = offer_order(
                StructureSet::OCEAN_MONUMENTS.structures,
                seed,
                chunk_x,
                chunk_z,
                |_| true,
            );
            assert_eq!(offered, vec![StructureKeys::Monument]);
            assert_eq!(selected, Some(StructureKeys::Monument));

            let (offered, selected) = offer_order(
                StructureSet::OCEAN_MONUMENTS.structures,
                seed,
                chunk_x,
                chunk_z,
                |_| false,
            );
            assert_eq!(offered, vec![StructureKeys::Monument]);
            assert_eq!(selected, None);
        }
    }

    /// The selection RNG is derived purely from (`world seed`, `chunk_x`,
    /// `chunk_z`) (`ChunkGenerator.java:407-408`), so repeating a selection for
    /// the same inputs must replay the identical candidate sequence: no hidden
    /// global RNG state, no iteration-order dependence, no leakage from a
    /// previous set's draws. A regression here would make a world non
    /// reproducible across saves/restarts.
    #[test]
    fn selection_is_deterministic_for_a_fixed_seed_and_chunk() {
        const SEED: i64 = -4_242_424_242;
        const CHUNK_X: i32 = 391;
        const CHUNK_Z: i32 = -1_207;

        // Vanilla oracle for (SEED, CHUNK_X, CHUNK_Z): the full exhaustion order
        // of every candidate, i.e. one bounded draw per failed candidate against
        // the shrinking total (`ChunkGenerator.java:415`, `:425-426`).
        let expected = [
            (
                StructureSet::MINESHAFTS.structures,
                vec![StructureKeys::MineshaftMesa, StructureKeys::Mineshaft],
            ),
            (
                StructureSet::NETHER_COMPLEXES.structures,
                vec![StructureKeys::BastionRemnant, StructureKeys::Fortress],
            ),
            (
                StructureSet::VILLAGES.structures,
                vec![
                    StructureKeys::VillageSavanna,
                    StructureKeys::VillageSnowy,
                    StructureKeys::VillageDesert,
                    StructureKeys::VillageTaiga,
                    StructureKeys::VillagePlains,
                ],
            ),
            (
                StructureSet::RUINED_PORTALS.structures,
                vec![
                    StructureKeys::RuinedPortal,
                    StructureKeys::RuinedPortalJungle,
                    StructureKeys::RuinedPortalMountain,
                    StructureKeys::RuinedPortalNether,
                    StructureKeys::RuinedPortalOcean,
                    StructureKeys::RuinedPortalDesert,
                    StructureKeys::RuinedPortalSwamp,
                ],
            ),
        ];

        for (entries, vanilla_order) in expected {
            for _ in 0..4 {
                // Every candidate fails: records the whole sequence, not just the
                // winner, so an extra or missing draw cannot hide behind an early
                // success.
                let (offered, selected) = offer_order(entries, SEED, CHUNK_X, CHUNK_Z, |_| false);
                assert_eq!(selected, None);
                assert_eq!(offered, vanilla_order);

                // Accepting only the last candidate walks the same sequence, and
                // accepting everything must stop at its first element.
                let last = *vanilla_order.last().expect("the set has candidates");
                let (offered, selected) =
                    offer_order(entries, SEED, CHUNK_X, CHUNK_Z, |key| key == last);
                assert_eq!(selected, Some(last));
                assert_eq!(offered, vanilla_order);

                let (offered, selected) = offer_order(entries, SEED, CHUNK_X, CHUNK_Z, |_| true);
                assert_eq!(selected, Some(vanilla_order[0]));
                assert_eq!(offered, vec![vanilla_order[0]]);
            }
        }
    }
}
