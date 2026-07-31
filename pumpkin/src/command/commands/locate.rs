//! `/locate` command.
//!
//! Mirrors vanilla `LocateCommand`
//! (`/root/Vanilla/src/net/minecraft/server/commands/LocateCommand.java`).
//!
//! Only the `structure` subcommand is registered. Vanilla also offers `biome`
//! and `poi`, but both need lookups Pumpkin does not have yet:
//!
//! - `biome` needs the equivalent of `ServerLevel.findClosestBiome3d`
//!   (`LocateCommand.java:93`), a standalone multi-noise climate probe that
//!   samples a 6400-block radius at 32/64 block resolution without loading
//!   chunks. Pumpkin's biome access (`Level::get_rough_biome`) only reads
//!   already-loaded chunks, so it cannot answer that query.
//! - `poi` needs a per-type POI index over the whole 256-block search square.
//!   `pumpkin_world::poi::PoiStorage` can answer that shape of query, but the
//!   only POI index the server actually populates is the nether-portal one, so
//!   registering the subcommand would report "not found" for every job site,
//!   bed or beehive.
//!
//! Registering either one now would mean shipping a command that silently
//! always fails, so they stay out until the underlying lookups exist.
//!
//! Known limitation inherited from `find_nearest_structure`: vanilla filters
//! candidates through `StructureManager.checkStructurePresence` and the
//! structure's biome predicate (`ChunkGenerator.java:239-241`), so it only
//! reports positions where the structure really starts. Pumpkin's finder
//! returns the position placement math *predicts*, without that confirmation,
//! so a reported position can be a candidate chunk that would not actually
//! generate the structure. This affects the existing eye-of-ender caller the
//! same way and is not introduced here.

use pumpkin_data::structures::StructureSet;
use pumpkin_data::translation;
use pumpkin_util::PermissionLvl;
use pumpkin_util::identifier::Identifier;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::permission::{Permission, PermissionDefault, PermissionRegistry};
use pumpkin_util::text::click::ClickEvent;
use pumpkin_util::text::hover::HoverEvent;
use pumpkin_util::text::{TextComponent, color::NamedColor};
use pumpkin_world::generation::generator::structure_finder::find_nearest_structure;
use std::borrow::Cow;

use crate::command::argument_builder::{ArgumentBuilder, argument, command, literal};
use crate::command::argument_types::FromStringReader;
use crate::command::argument_types::argument_type::{ArgumentType, JavaClientArgumentType};
use crate::command::context::command_context::CommandContext;
use crate::command::errors::command_syntax_error::CommandSyntaxError;
use crate::command::errors::error_types::CommandErrorType;
use crate::command::node::dispatcher::CommandDispatcher;
use crate::command::node::{CommandExecutor, CommandExecutorResult};
use crate::command::string_reader::StringReader;
use crate::command::suggestion::suggestions::{Suggestions, SuggestionsBuilder};
use std::pin::Pin;

const DESCRIPTION: &str = "Locates the closest structure of a given type.";
const PERMISSION: &str = "minecraft:command.locate";

const ARG_STRUCTURE: &str = "structure";

/// The structure-set names accepted by [`StructureSet::get`] in the generated
/// structure data. Kept here purely to drive tab-completion; parsing still goes
/// through `StructureSet::get` so the two can never disagree on what is valid
/// (the `structure_set_names_all_resolve` test enforces that).
const STRUCTURE_SET_NAMES: &[&str] = &[
    "ancient_cities",
    "buried_treasures",
    "desert_pyramids",
    "end_cities",
    "igloos",
    "jungle_temples",
    "mineshafts",
    "nether_complexes",
    "nether_fossils",
    "ocean_monuments",
    "ocean_ruins",
    "pillager_outposts",
    "ruined_portals",
    "shipwrecks",
    "strongholds",
    "swamp_huts",
    "trail_ruins",
    "trial_chambers",
    "villages",
    "woodland_mansions",
];

/// Parses the structure argument as a plain [`Identifier`], adding
/// tab-completion over the known structure-set names.
///
/// Vanilla uses `ResourceOrTagKeyArgument.resourceOrTagKey(Registries.STRUCTURE)`
/// (`LocateCommand.java:69`), which suggests from the structure registry and
/// also accepts `#tag` forms. Pumpkin has no runtime structure registry to
/// enumerate, so suggestions come from the structure-set table instead and tags
/// are not accepted.
struct StructureArgumentType;

impl ArgumentType for StructureArgumentType {
    type Item = Identifier;

    fn parse(&self, reader: &mut StringReader) -> Result<Self::Item, CommandSyntaxError> {
        Identifier::from_reader(reader)
    }

    fn list_suggestions<'a>(
        &'a self,
        _context: &'a CommandContext,
        builder: SuggestionsBuilder,
    ) -> Pin<Box<dyn Future<Output = Suggestions> + Send + 'a>> {
        Box::pin(async move {
            builder
                .filter_and_suggest_iter(
                    STRUCTURE_SET_NAMES
                        .iter()
                        .map(|name| format!("minecraft:{name}")),
                )
                .build()
        })
    }

    fn client_side_parser(&'_ self) -> JavaClientArgumentType {
        JavaClientArgumentType::ResourceLocation
    }
}

/// `LocateCommand.MAX_STRUCTURE_SEARCH_RADIUS` (`LocateCommand.java:62`), in
/// chunks. Passed straight through to `find_nearest_structure`, which counts
/// its `max_search_radius` in placement rings just like vanilla
/// `ChunkGenerator.findNearestMapStructure`.
const MAX_STRUCTURE_SEARCH_RADIUS: i32 = 100;

/// `LocateCommand.ERROR_STRUCTURE_NOT_FOUND` (`LocateCommand.java:58`).
static ERROR_STRUCTURE_NOT_FOUND: CommandErrorType<1> = CommandErrorType::new(
    translation::java::COMMANDS_LOCATE_STRUCTURE_NOT_FOUND,
    translation::java::COMMANDS_LOCATE_STRUCTURE_NOT_FOUND,
);

/// `LocateCommand.ERROR_STRUCTURE_INVALID` (`LocateCommand.java:59`).
static ERROR_STRUCTURE_INVALID: CommandErrorType<1> = CommandErrorType::new(
    translation::java::COMMANDS_LOCATE_STRUCTURE_INVALID,
    translation::java::COMMANDS_LOCATE_STRUCTURE_INVALID,
);

/// Vanilla takes a structure *or* a structure tag and resolves it against the
/// structure registry (`LocateCommand.java:72-74`). Pumpkin's generated
/// structure data is keyed by structure **set** (`StructureSet::get`), which is
/// the granularity `find_nearest_structure` needs anyway, so the argument is
/// resolved against those set names.
fn resolve_structure_set(name: &str) -> Option<&'static StructureSet> {
    StructureSet::get(name)
}

/// Vanilla's horizontal-only distance helper (`LocateCommand.java:133-137`).
///
/// `showLocateResult` picks this over the 3D distance whenever `includeY` is
/// false (`LocateCommand.java:125`), which is the case for the `structure`
/// subcommand (`LocateCommand.java:87`).
fn horizontal_distance(from: BlockPos, to: BlockPos) -> i32 {
    let dx = f64::from(to.0.x - from.0.x);
    let dz = f64::from(to.0.z - from.0.z);
    // `Mth.floor(Mth.sqrt(...))` in vanilla.
    dx.hypot(dz).floor() as i32
}

/// Vanilla `LocateCommand.showLocateResult` (`LocateCommand.java:123-131`).
///
/// The Y coordinate is displayed as `~` because the `structure` subcommand
/// passes `includeY = false` (`LocateCommand.java:87`); the found position's Y
/// is not meaningful for a structure start.
fn build_success_message(found_name: &str, found_pos: BlockPos, distance: i32) -> TextComponent {
    let displayed_y = "~";
    let teleport_command = format!("/tp @s {} {displayed_y} {}", found_pos.0.x, found_pos.0.z);

    let coordinates = TextComponent::translate_cross(
        translation::java::CHAT_COORDINATES,
        translation::java::CHAT_COORDINATES,
        [
            TextComponent::text(found_pos.0.x.to_string()),
            TextComponent::text(displayed_y),
            TextComponent::text(found_pos.0.z.to_string()),
        ],
    )
    .wrap_in_square_brackets()
    .color_named(NamedColor::Green)
    .click_event(ClickEvent::SuggestCommand {
        command: Cow::from(teleport_command),
    })
    .hover_event(HoverEvent::show_text(TextComponent::translate_cross(
        translation::java::CHAT_COORDINATES_TOOLTIP,
        translation::java::CHAT_COORDINATES_TOOLTIP,
        [],
    )));

    TextComponent::translate_cross(
        translation::java::COMMANDS_LOCATE_STRUCTURE_SUCCESS,
        translation::java::COMMANDS_LOCATE_STRUCTURE_SUCCESS,
        [
            TextComponent::text(found_name.to_string()),
            coordinates,
            TextComponent::text(distance.to_string()),
        ],
    )
}

struct LocateStructureExecutor;

impl CommandExecutor for LocateStructureExecutor {
    fn execute<'a>(&'a self, context: &'a CommandContext) -> CommandExecutorResult<'a> {
        Box::pin(async move {
            let identifier = context.get_argument::<Identifier>(ARG_STRUCTURE)?;
            let structure_name = identifier.path().to_string();

            // `LocateCommand.java:78`: an unresolvable structure is a hard error,
            // distinct from "searched and found nothing".
            let structure_set = resolve_structure_set(&structure_name).ok_or_else(|| {
                ERROR_STRUCTURE_INVALID
                    .create_without_context(TextComponent::text(structure_name.clone()))
            })?;

            // `LocateCommand.java:79`: search starts from the sender's position.
            // `BlockPos.containing` floors, so use the flooring constructor
            // rather than `to_block_pos`, which rounds.
            let origin = BlockPos::floored_v(context.source.position);

            let world = context.world();
            let level = &world.level;
            let seed = level.seed.0;

            // Superflat worlds have no structure placement cache, so there is
            // nothing to search; report it the same way vanilla reports a
            // failed search.
            let found = level
                .world_gen
                .global_structure_cache()
                .and_then(|global_cache| {
                    find_nearest_structure(
                        origin,
                        &[&structure_set.placement],
                        MAX_STRUCTURE_SEARCH_RADIUS,
                        seed as i64,
                        global_cache,
                    )
                });

            // `LocateCommand.java:84-86`.
            let Some(found_pos) = found else {
                return Err(ERROR_STRUCTURE_NOT_FOUND
                    .create_without_context(TextComponent::text(structure_name)));
            };

            let distance = horizontal_distance(origin, found_pos);

            context
                .source
                .send_feedback(
                    build_success_message(&structure_name, found_pos, distance),
                    false,
                )
                .await;

            // `LocateCommand.java:130`: the command result is the distance.
            Ok(distance)
        })
    }
}

pub fn register(dispatcher: &mut CommandDispatcher, registry: &mut PermissionRegistry) {
    registry.register_permission_or_panic(Permission::new(
        PERMISSION,
        DESCRIPTION,
        // `LocateCommand.java:69`: `Commands.LEVEL_GAMEMASTERS`, i.e. level 2.
        PermissionDefault::Op(PermissionLvl::Two),
    ));

    dispatcher.register(
        command("locate", DESCRIPTION)
            .requires(PERMISSION)
            .then(literal("structure").then(
                argument(ARG_STRUCTURE, StructureArgumentType).executes(LocateStructureExecutor),
            )),
    );
}

#[cfg(test)]
mod test {
    use super::{STRUCTURE_SET_NAMES, horizontal_distance, resolve_structure_set};
    use pumpkin_util::math::position::BlockPos;

    #[test]
    fn structure_set_names_all_resolve() {
        // Every suggested name must actually parse, otherwise tab-completion
        // would offer values the executor then rejects as invalid.
        for name in STRUCTURE_SET_NAMES {
            assert!(
                resolve_structure_set(name).is_some(),
                "suggested structure set '{name}' does not resolve"
            );
        }
    }

    #[test]
    fn resolves_known_structure_sets() {
        // Names come from `StructureSet::get` in the generated structure data.
        assert!(resolve_structure_set("strongholds").is_some());
        assert!(resolve_structure_set("villages").is_some());
        assert!(resolve_structure_set("ancient_cities").is_some());
    }

    #[test]
    fn rejects_unknown_structure_sets() {
        assert!(resolve_structure_set("not_a_structure").is_none());
        // The registry is keyed by set name, not by singular structure name.
        assert!(resolve_structure_set("").is_none());
    }

    #[test]
    fn horizontal_distance_ignores_y() {
        let origin = BlockPos::new(0, 64, 0);
        // Y differences must not affect the reported distance, matching
        // vanilla's `includeY = false` path for `/locate structure`.
        assert_eq!(horizontal_distance(origin, BlockPos::new(0, -320, 0)), 0);
        assert_eq!(horizontal_distance(origin, BlockPos::new(3, 1000, 4)), 5);
    }

    #[test]
    fn horizontal_distance_floors_like_vanilla() {
        let origin = BlockPos::new(0, 0, 0);
        // sqrt(2) = 1.41... floors to 1.
        assert_eq!(horizontal_distance(origin, BlockPos::new(1, 0, 1)), 1);
        // sqrt(200) = 14.14... floors to 14.
        assert_eq!(horizontal_distance(origin, BlockPos::new(10, 0, 10)), 14);
        // Direction must not matter.
        assert_eq!(horizontal_distance(origin, BlockPos::new(-10, 0, -10)), 14);
    }
}
