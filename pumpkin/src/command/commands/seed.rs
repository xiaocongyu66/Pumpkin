use crate::command::argument_builder::{ArgumentBuilder, command};
use crate::command::context::command_context::CommandContext;
use crate::command::node::dispatcher::CommandDispatcher;
use crate::command::node::{CommandExecutor, CommandExecutorResult};
use pumpkin_data::translation;
use pumpkin_util::PermissionLvl;
use pumpkin_util::permission::{Permission, PermissionDefault, PermissionRegistry};
use pumpkin_util::text::click::ClickEvent;
use pumpkin_util::text::hover::HoverEvent;
use pumpkin_util::text::{TextComponent, color::NamedColor};
use std::borrow::Cow;

const DESCRIPTION: &str = "Displays the world seed.";
const PERMISSION: &str = "minecraft:command.seed";

struct SeedCommandExecutor;

/// Formats a world seed the way vanilla does.
///
/// `Seed` stores the seed as a `u64`, but its semantics are those of a Java
/// `long`: `SeedCommand.java:22` calls `String.valueOf(long)` on the value
/// returned by `ServerLevel#getSeed`. Reinterpreting the bit pattern as `i64`
/// keeps negative seeds negative instead of printing them as huge unsigned
/// numbers, which would send players to a completely different world.
fn format_seed(seed: u64) -> String {
    (seed as i64).to_string()
}

fn create_copy_on_click_text(content: String) -> TextComponent {
    TextComponent::translate_cross(
        translation::java::COMMANDS_SEED_SUCCESS,
        translation::bedrock::COMMANDS_SEED_SUCCESS,
        [TextComponent::wrap_in_square_brackets(
            TextComponent::text(content.clone())
                .hover_event(HoverEvent::show_text(TextComponent::translate_cross(
                    translation::java::CHAT_COPY_CLICK,
                    translation::java::CHAT_COPY_CLICK,
                    [],
                )))
                .click_event(ClickEvent::CopyToClipboard {
                    value: Cow::from(content),
                })
                .color_named(NamedColor::Green),
        )],
    )
}

impl CommandExecutor for SeedCommandExecutor {
    fn execute<'a>(&'a self, context: &'a CommandContext) -> CommandExecutorResult<'a> {
        Box::pin(async move {
            let seed = context.world().level.seed.0;

            context
                .source
                .send_feedback(create_copy_on_click_text(format_seed(seed)), false)
                .await;

            // `SeedCommand.java:24` returns `(int)seed`: the low 32 bits of the
            // seed, reinterpreted as signed. Rust's `as` cast truncates and
            // reinterprets the same way, so this matches vanilla exactly.
            Ok(seed as i32)
        })
    }
}

pub fn register(dispatcher: &mut CommandDispatcher, registry: &mut PermissionRegistry) {
    registry.register_permission_or_panic(Permission::new(
        PERMISSION,
        DESCRIPTION,
        // For integrated servers, the permission level is 0,
        // but Pumpkin is always a dedicated server. For dedicated servers,
        // /seed is limited to level 2.
        PermissionDefault::Op(PermissionLvl::Two),
    ));

    dispatcher.register(
        command("seed", DESCRIPTION)
            .requires(PERMISSION)
            .executes(SeedCommandExecutor),
    );
}

#[cfg(test)]
mod tests {
    use super::format_seed;
    use pumpkin_util::world_seed::Seed;

    #[test]
    fn negative_seed_is_displayed_as_signed() {
        // `Seed` keeps the Java `long` bit pattern in a `u64`. Printing it as
        // unsigned would yield 14274599075807261974, which recreates a
        // completely different world.
        let seed = Seed::from("-4172144997902289642");
        assert_eq!(seed.0, 14274599075807261974);
        assert_eq!(format_seed(seed.0), "-4172144997902289642");
    }

    #[test]
    fn positive_seed_is_unchanged() {
        let seed = Seed::from("2151901553968352745");
        assert_eq!(format_seed(seed.0), "2151901553968352745");
    }

    #[test]
    fn seed_bounds_round_trip() {
        assert_eq!(format_seed(0), "0");
        assert_eq!(format_seed(u64::MAX), "-1");
        assert_eq!(format_seed(i64::MIN as u64), i64::MIN.to_string());
        assert_eq!(format_seed(i64::MAX as u64), i64::MAX.to_string());
    }

    #[test]
    fn command_result_matches_vanilla_int_cast() {
        // `SeedCommand.java:24` returns `(int)seed`.
        let seed = Seed::from("-4172144997902289642");
        assert_eq!(seed.0 as i32, -1197705962);
    }
}
