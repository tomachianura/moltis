//! Native Discord slash command registration and handling.
//!
//! Registers global application commands (/new, /model, /help, etc.) when the
//! bot connects, and dispatches interactions through the same `dispatch_command`
//! path used by text-based `/` commands.

use {
    serenity::all::{
        Command, CommandDataOption, CommandDataOptionValue, CommandInteraction, CommandOptionType,
        ComponentInteraction, Context, CreateCommand, CreateCommandOption,
        CreateInteractionResponse, CreateInteractionResponseFollowup,
        CreateInteractionResponseMessage, EditInteractionResponse, GuildId, Interaction,
        InteractionContext,
    },
    tracing::{debug, info, warn},
};

use crate::{
    access::{self, AccessDenied},
    config::DiscordAccountConfig,
    state::AccountStateMap,
};

/// Ephemeral reply to an interaction refused by the DM access policy.
const DM_ACCESS_DENIED_MSG: &str = "You are not allowed to use this bot in direct messages.";

/// Whether an interaction was invoked in the bot's own one-to-one DM.
///
/// A missing `guild_id` is not enough on its own: a user-installed app can be
/// invoked from the user's DMs and group DMs with other people
/// (`PrivateChannel`). Only an explicit `BotDm` context proves the bot is the
/// other party, and a missing context fails closed.
pub(crate) fn is_bot_dm_interaction(
    guild_id: Option<GuildId>,
    context: Option<InteractionContext>,
) -> bool {
    guild_id.is_none() && context == Some(InteractionContext::BotDm)
}

/// Reply target for a slash-command or component interaction, forwarding
/// whether it came from the bot's DM so the gateway can classify it as direct.
pub(crate) fn interaction_reply_target(
    account_id: &str,
    channel_id: serenity::all::ChannelId,
    guild_id: Option<GuildId>,
    context: Option<InteractionContext>,
) -> moltis_channels::plugin::ChannelReplyTarget {
    moltis_channels::plugin::ChannelReplyTarget {
        direct_chat: is_bot_dm_interaction(guild_id, context),
        ack_message_id: None,
        channel_type: moltis_channels::ChannelType::Discord,
        account_id: account_id.to_string(),
        chat_id: channel_id.to_string(),
        message_id: None,
        thread_id: None,
    }
}

/// Decide whether an interaction may reach the gateway, and build its reply
/// target.
///
/// An interaction in the bot's DM is forwarded as a direct chat, so it must
/// first pass the same DM access check (`dm_policy` and the DM allowlist) that
/// a direct message to the bot does; otherwise a user barred from messaging
/// the bot could still run operator direct-chat commands through a slash
/// command or a button. Interactions outside the bot's DM are not direct and
/// keep their existing behaviour.
pub(crate) fn authorize_interaction(
    account_id: &str,
    config: &DiscordAccountConfig,
    channel_id: serenity::all::ChannelId,
    guild_id: Option<GuildId>,
    context: Option<InteractionContext>,
    user_id: &str,
    username: &str,
) -> Result<moltis_channels::plugin::ChannelReplyTarget, AccessDenied> {
    if is_bot_dm_interaction(guild_id, context) {
        access::check_access(
            config,
            &moltis_common::types::ChatType::Dm,
            user_id,
            Some(username),
            None,
            false,
        )?;
    }
    Ok(interaction_reply_target(
        account_id, channel_id, guild_id, context,
    ))
}

/// Build the set of global slash commands to register.
///
/// Derives from the centralized command registry in `moltis_channels::commands`.
pub fn build_commands() -> Vec<CreateCommand> {
    moltis_channels::commands::all_commands()
        .iter()
        .map(|c| {
            let mut cmd = CreateCommand::new(c.name).description(c.description);
            if let Some(arg) = &c.arg {
                let mut opt =
                    CreateCommandOption::new(CommandOptionType::String, arg.name, arg.description)
                        .required(arg.required);
                for &(label, value) in arg.choices {
                    opt = opt.add_string_choice(label, value);
                }
                cmd = cmd.add_option(opt);
            }
            cmd
        })
        .collect()
}

/// Register global slash commands for the bot.
pub async fn register_global_commands(ctx: &Context, account_id: &str) {
    match Command::set_global_commands(&ctx, build_commands()).await {
        Ok(commands) => {
            let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
            info!(
                account_id,
                commands = ?names,
                "Registered {} Discord slash commands",
                commands.len()
            );
        },
        Err(e) => {
            warn!(account_id, "Failed to register Discord slash commands: {e}");
        },
    }
}

/// Handle an incoming interaction (slash command, button click, etc.).
pub async fn handle_interaction(
    ctx: &Context,
    interaction: &Interaction,
    account_id: &str,
    accounts: &AccountStateMap,
) {
    match interaction {
        Interaction::Command(command) => {
            handle_slash_command(ctx, command, account_id, accounts).await;
        },
        Interaction::Component(component) => {
            handle_component_interaction(ctx, component, account_id, accounts).await;
        },
        _ => {},
    }
}

/// Handle a slash command interaction.
async fn handle_slash_command(
    ctx: &Context,
    command: &CommandInteraction,
    account_id: &str,
    accounts: &AccountStateMap,
) {
    debug!(
        account_id,
        command = %command.data.name,
        user = %command.user.name,
        "Discord slash command received"
    );

    let sender_id = command.user.id.to_string();
    let state = {
        let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
        accts
            .get(account_id)
            .map(|s| (s.config.clone(), s.event_sink.clone()))
    };

    // Check DM access before acknowledging, so a refused command never reaches
    // the gateway and the refusal is the interaction's only response.
    let reply_to = match state.as_ref().map(|(config, _)| {
        authorize_interaction(
            account_id,
            config,
            command.channel_id,
            command.guild_id,
            command.context,
            &sender_id,
            &command.user.name,
        )
    }) {
        Some(Ok(reply_to)) => Some(reply_to),
        Some(Err(reason)) => {
            info!(
                account_id,
                command = %command.data.name,
                user_id = %sender_id,
                %reason,
                "Discord slash command refused by DM access policy"
            );
            refuse_interaction(command.create_response(ctx, refusal_response())).await;
            return;
        },
        None => None,
    };

    if let Err(e) = command.defer_ephemeral(ctx).await {
        warn!(
            command = %command.data.name,
            "Failed to acknowledge slash command: {e}"
        );
        return;
    }

    let (Some(reply_to), Some(sink)) = (reply_to, state.and_then(|(_, sink)| sink)) else {
        respond_ephemeral(ctx, command, "Bot is not ready yet.").await;
        return;
    };

    let command_text = build_command_text(&command.data.name, &command.data.options);

    let response_text = match sink
        .dispatch_command(&command_text, reply_to, Some(&sender_id))
        .await
    {
        Ok(response) => response,
        Err(e) => format!("Command failed: {e}"),
    };

    respond_ephemeral(ctx, command, &response_text).await;
}

/// Handle a component (button click) interaction.
async fn handle_component_interaction(
    ctx: &Context,
    component: &ComponentInteraction,
    account_id: &str,
    accounts: &AccountStateMap,
) {
    let callback_data = &component.data.custom_id;

    debug!(
        account_id,
        callback_data,
        user = %component.user.name,
        "Discord component interaction received"
    );

    let sender_id = component.user.id.to_string();
    let state = {
        let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
        accts
            .get(account_id)
            .map(|s| (s.config.clone(), s.event_sink.clone()))
    };
    let Some((config, event_sink)) = state else {
        return;
    };

    // Check DM access before acknowledging, so a refused interaction never
    // reaches the gateway and the refusal is its only response.
    let reply_to = match authorize_interaction(
        account_id,
        &config,
        component.channel_id,
        component.guild_id,
        component.context,
        &sender_id,
        &component.user.name,
    ) {
        Ok(reply_to) => reply_to,
        Err(reason) => {
            info!(
                account_id,
                callback_data,
                user_id = %sender_id,
                %reason,
                "Discord component interaction refused by DM access policy"
            );
            refuse_interaction(component.create_response(ctx, refusal_response())).await;
            return;
        },
    };

    // Acknowledge the interaction immediately so Discord doesn't show a failure.
    if let Err(e) = component
        .create_response(ctx, CreateInteractionResponse::Acknowledge)
        .await
    {
        warn!(
            account_id,
            callback_data, "Failed to acknowledge component interaction: {e}"
        );
        return;
    }

    let Some(sink) = event_sink else {
        return;
    };

    match sink
        .dispatch_interaction(callback_data, reply_to, Some(&sender_id))
        .await
    {
        Ok(_response) => {
            // Response already sent by the gateway.
        },
        Err(e) => {
            debug!(
                account_id,
                callback_data, "interaction dispatch failed: {e}"
            );
        },
    }
}

/// Build the full command string from the slash command name and its options.
///
/// Discord slash commands pass arguments as structured options rather than
/// inline text. This reconstructs the `"name value"` format expected by
/// `dispatch_command`.
fn build_command_text(name: &str, options: &[CommandDataOption]) -> String {
    let arg = options.iter().find_map(|opt| match &opt.value {
        CommandDataOptionValue::String(s) => Some(s.as_str()),
        _ => None,
    });

    match arg {
        Some(value) if !value.is_empty() => format!("{name} {value}"),
        _ => name.to_string(),
    }
}

/// Ephemeral refusal for an interaction denied by the DM access policy.
fn refusal_response() -> CreateInteractionResponse {
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(DM_ACCESS_DENIED_MSG)
            .ephemeral(true),
    )
}

/// Send a refusal, logging (not propagating) a failure to deliver it.
async fn refuse_interaction(send: impl Future<Output = serenity::Result<()>>) {
    if let Err(e) = send.await {
        warn!("Failed to send DM access refusal: {e}");
    }
}

/// Send an ephemeral response to a slash command (only visible to the invoker).
async fn respond_ephemeral(ctx: &Context, command: &CommandInteraction, text: &str) {
    if let Err(e) = command
        .edit_response(&ctx, EditInteractionResponse::new().content(text))
        .await
    {
        warn!(
            command = %command.data.name,
            "Failed to edit deferred slash response: {e}"
        );
        if let Err(followup_err) = command
            .create_followup(
                &ctx,
                CreateInteractionResponseFollowup::new()
                    .content(text)
                    .ephemeral(true),
            )
            .await
        {
            warn!(
                command = %command.data.name,
                "Failed to send slash follow-up response: {followup_err}"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn bot_dm_interaction_forwards_direct_chat() {
        let target = interaction_reply_target(
            "bot",
            serenity::all::ChannelId::new(42),
            None,
            Some(InteractionContext::BotDm),
        );
        assert!(target.direct_chat);
        assert_eq!(target.chat_id, "42");
        assert!(!target.is_shared_chat());
    }

    #[test]
    fn non_bot_dm_interactions_stay_shared() {
        let channel = serenity::all::ChannelId::new(42);
        let guild = Some(GuildId::new(9));
        for (guild_id, context) in [
            (guild, Some(InteractionContext::Guild)),
            // A user-installed app invoked in a DM or group DM between people.
            (None, Some(InteractionContext::PrivateChannel)),
            // Older payloads without a context field.
            (None, None),
            // A guild id always means a guild, whatever the context claims.
            (guild, Some(InteractionContext::BotDm)),
        ] {
            let target = interaction_reply_target("bot", channel, guild_id, context);
            assert!(
                !target.direct_chat,
                "guild={guild_id:?} context={context:?} was forwarded as direct"
            );
            assert!(target.is_shared_chat());
        }
    }

    const OPERATOR_ID: &str = "400347514466992128";
    const STRANGER_ID: &str = "999999999";

    fn dm_config(policy: moltis_channels::gating::DmPolicy) -> DiscordAccountConfig {
        DiscordAccountConfig {
            dm_policy: policy,
            allowlist: vec![OPERATOR_ID.into()],
            ..DiscordAccountConfig::default()
        }
    }

    /// Authorize an interaction the way both the slash-command and the
    /// component handler do.
    fn authorize(
        config: &DiscordAccountConfig,
        guild_id: Option<GuildId>,
        context: Option<InteractionContext>,
        user_id: &str,
    ) -> Result<moltis_channels::plugin::ChannelReplyTarget, AccessDenied> {
        authorize_interaction(
            "bot",
            config,
            serenity::all::ChannelId::new(42),
            guild_id,
            context,
            user_id,
            "someone",
        )
    }

    #[test]
    fn bot_dm_interaction_is_refused_when_dms_are_disabled() {
        let config = dm_config(moltis_channels::gating::DmPolicy::Disabled);
        // Even a user on the allowlist (e.g. an operator) is refused.
        for user in [OPERATOR_ID, STRANGER_ID] {
            assert_eq!(
                authorize(&config, None, Some(InteractionContext::BotDm), user).err(),
                Some(AccessDenied::DmsDisabled),
            );
        }
    }

    #[test]
    fn bot_dm_interaction_is_refused_for_user_off_the_allowlist() {
        let config = dm_config(moltis_channels::gating::DmPolicy::Allowlist);
        assert_eq!(
            authorize(&config, None, Some(InteractionContext::BotDm), STRANGER_ID).err(),
            Some(AccessDenied::NotOnAllowlist),
        );

        let empty = DiscordAccountConfig {
            allowlist: Vec::new(),
            ..config
        };
        assert_eq!(
            authorize(&empty, None, Some(InteractionContext::BotDm), OPERATOR_ID).err(),
            Some(AccessDenied::NotOnAllowlist),
        );
    }

    #[test]
    fn bot_dm_interaction_matches_allowlist_by_username() {
        let config = DiscordAccountConfig {
            allowlist: vec!["someone".into()],
            ..dm_config(moltis_channels::gating::DmPolicy::Allowlist)
        };
        let target = authorize(&config, None, Some(InteractionContext::BotDm), STRANGER_ID)
            .expect("allowlisted username is allowed");
        assert!(target.direct_chat);
    }

    #[test]
    fn allowed_bot_dm_interaction_is_direct() {
        for policy in [
            moltis_channels::gating::DmPolicy::Allowlist,
            moltis_channels::gating::DmPolicy::Open,
        ] {
            let config = dm_config(policy.clone());
            let target = authorize(&config, None, Some(InteractionContext::BotDm), OPERATOR_ID)
                .unwrap_or_else(|e| panic!("{policy:?}: allowed operator refused: {e}"));
            assert!(target.direct_chat, "{policy:?}");
            assert!(!target.is_shared_chat(), "{policy:?}");
        }
    }

    #[test]
    fn dm_policy_does_not_change_interactions_outside_the_bot_dm() {
        let guild = Some(GuildId::new(9));
        for policy in [
            moltis_channels::gating::DmPolicy::Disabled,
            moltis_channels::gating::DmPolicy::Allowlist,
        ] {
            let config = dm_config(policy.clone());
            for (guild_id, context) in [
                (guild, Some(InteractionContext::Guild)),
                (guild, Some(InteractionContext::BotDm)),
                (None, Some(InteractionContext::PrivateChannel)),
                (None, None),
            ] {
                let target =
                    authorize(&config, guild_id, context, STRANGER_ID).unwrap_or_else(|e| {
                        panic!("{policy:?} guild={guild_id:?} context={context:?}: {e}")
                    });
                assert!(
                    !target.direct_chat,
                    "{policy:?} guild={guild_id:?} context={context:?} was forwarded as direct"
                );
                assert!(target.is_shared_chat());
            }
        }
    }

    #[test]
    fn build_commands_matches_registry_count() {
        let commands = build_commands();
        let registry_count = moltis_channels::commands::all_commands().len();
        assert_eq!(
            commands.len(),
            registry_count,
            "build_commands should produce one command per registry entry"
        );
    }

    #[test]
    fn build_commands_serializes_to_valid_json() {
        let commands = build_commands();
        // Each CreateCommand should serialize successfully (validates structure).
        for cmd in &commands {
            let json = serde_json::to_value(cmd)
                .unwrap_or_else(|e| panic!("failed to serialize command: {e}"));
            // Verify name field is present and non-empty.
            let name = json["name"].as_str().unwrap_or_default();
            assert!(!name.is_empty(), "command name is empty");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
                "invalid command name: {name}"
            );
            assert!(name.len() <= 32, "command name too long: {name}");
            // Verify description is present.
            let desc = json["description"].as_str().unwrap_or_default();
            assert!(!desc.is_empty(), "command {name} has empty description");
        }
    }

    #[test]
    fn no_duplicate_command_names() {
        let commands = build_commands();
        let mut names: Vec<String> = commands
            .iter()
            .filter_map(|c| {
                serde_json::to_value(c)
                    .ok()
                    .and_then(|v| v["name"].as_str().map(String::from))
            })
            .collect();
        let original_len = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            original_len,
            "duplicate slash command names found"
        );
    }

    #[test]
    fn all_registry_commands_present() {
        let commands = build_commands();
        let names: Vec<String> = commands
            .iter()
            .filter_map(|c| {
                serde_json::to_value(c)
                    .ok()
                    .and_then(|v| v["name"].as_str().map(String::from))
            })
            .collect();
        for cmd in moltis_channels::commands::all_commands() {
            assert!(
                names.contains(&cmd.name.to_string()),
                "missing slash command from registry: {}",
                cmd.name
            );
        }
    }

    #[test]
    fn descriptions_within_discord_limit() {
        // Discord enforces a 100-character limit on command descriptions.
        let commands = build_commands();
        for cmd in &commands {
            let json = serde_json::to_value(cmd)
                .unwrap_or_else(|e| panic!("failed to serialize command: {e}"));
            let name = json["name"].as_str().unwrap_or("unknown");
            let desc = json["description"].as_str().unwrap_or_default();
            assert!(
                desc.len() <= 100,
                "command {name} description exceeds 100 chars ({} chars): {desc}",
                desc.len()
            );
        }
    }

    #[test]
    fn command_names_are_lowercase_alphanumeric() {
        // Discord requires command names to be lowercase with no spaces.
        let commands = build_commands();
        for cmd in &commands {
            let json = serde_json::to_value(cmd)
                .unwrap_or_else(|e| panic!("failed to serialize command: {e}"));
            let name = json["name"].as_str().unwrap_or_default();
            assert!(
                !name.is_empty() && name.len() <= 32,
                "command name length out of range: {name}"
            );
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
                "command name contains invalid characters: {name}"
            );
        }
    }

    #[test]
    fn commands_with_arg_have_options() {
        let commands = build_commands();
        let registry = moltis_channels::commands::all_commands();

        for reg_cmd in registry {
            let json = commands
                .iter()
                .find_map(|c| {
                    let v = serde_json::to_value(c).ok()?;
                    if v["name"].as_str()? == reg_cmd.name {
                        Some(v)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| panic!("missing built command: {}", reg_cmd.name));

            let options = json["options"].as_array();

            if let Some(arg) = &reg_cmd.arg {
                let opts = options.unwrap_or_else(|| {
                    panic!("command /{} has arg but no Discord options", reg_cmd.name)
                });
                assert!(
                    !opts.is_empty(),
                    "command /{} has arg but empty options array",
                    reg_cmd.name
                );
                let first = &opts[0];
                assert_eq!(
                    first["name"].as_str(),
                    Some(arg.name),
                    "command /{} option name should be \"{}\"",
                    reg_cmd.name,
                    arg.name,
                );
                // CommandOptionType::String == 3
                assert_eq!(
                    first["type"].as_u64(),
                    Some(3),
                    "command /{} option type should be String (3)",
                    reg_cmd.name
                );

                // Verify choices match
                if !arg.choices.is_empty() {
                    let json_choices = first["choices"].as_array().unwrap_or_else(|| {
                        panic!("command /{} has choices but none in JSON", reg_cmd.name)
                    });
                    assert_eq!(
                        json_choices.len(),
                        arg.choices.len(),
                        "command /{} choice count mismatch",
                        reg_cmd.name
                    );
                    for (json_choice, &(label, value)) in
                        json_choices.iter().zip(arg.choices.iter())
                    {
                        assert_eq!(json_choice["name"].as_str(), Some(label));
                        assert_eq!(json_choice["value"].as_str(), Some(value));
                    }
                }
            } else {
                let is_empty = options.is_none_or(|o| o.is_empty());
                assert!(
                    is_empty,
                    "command /{} has no arg but has Discord options",
                    reg_cmd.name
                );
            }
        }
    }

    fn string_option(value: &str) -> CommandDataOption {
        serde_json::from_value(serde_json::json!({
            "name": "value",
            "type": 3,
            "value": value,
        }))
        .expect("valid string option")
    }

    fn bool_option(value: bool) -> CommandDataOption {
        serde_json::from_value(serde_json::json!({
            "name": "value",
            "type": 5,
            "value": value,
        }))
        .expect("valid bool option")
    }

    #[test]
    fn build_command_text_no_options() {
        let text = build_command_text("new", &[]);
        assert_eq!(text, "new");
    }

    #[test]
    fn build_command_text_with_string_option() {
        let options = vec![string_option("2")];
        let text = build_command_text("mode", &options);
        assert_eq!(text, "mode 2");
    }

    #[test]
    fn build_command_text_with_empty_string_option() {
        let options = vec![string_option("")];
        let text = build_command_text("mode", &options);
        assert_eq!(text, "mode");
    }

    #[test]
    fn build_command_text_ignores_non_string_options() {
        let options = vec![bool_option(true)];
        let text = build_command_text("fast", &options);
        assert_eq!(text, "fast");
    }

    #[test]
    fn build_command_text_with_multi_word_arg() {
        let options = vec![string_option("provider:openai gpt-4o")];
        let text = build_command_text("model", &options);
        assert_eq!(text, "model provider:openai gpt-4o");
    }

    #[test]
    fn option_descriptions_within_discord_limit() {
        // Discord enforces a 100-character limit on option descriptions too.
        for cmd in moltis_channels::commands::all_commands() {
            if let Some(arg) = &cmd.arg {
                assert!(
                    arg.description.len() <= 100,
                    "command /{} arg description exceeds 100 chars ({} chars): {}",
                    cmd.name,
                    arg.description.len(),
                    arg.description,
                );
            }
        }
    }

    #[test]
    fn arg_names_are_valid_discord_option_names() {
        // Discord option names: lowercase, 1-32 chars, alphanumeric + hyphens.
        for cmd in moltis_channels::commands::all_commands() {
            if let Some(arg) = &cmd.arg {
                assert!(
                    !arg.name.is_empty() && arg.name.len() <= 32,
                    "command /{} arg name length out of range: {}",
                    cmd.name,
                    arg.name,
                );
                assert!(
                    arg.name.chars().all(|c| c.is_ascii_lowercase()
                        || c.is_ascii_digit()
                        || c == '-'
                        || c == '_'),
                    "command /{} arg name has invalid characters: {}",
                    cmd.name,
                    arg.name,
                );
            }
        }
    }

    #[test]
    fn choices_within_discord_limits() {
        // Discord allows max 25 choices, each name ≤ 100 chars, each value ≤ 100 chars.
        for cmd in moltis_channels::commands::all_commands() {
            if let Some(arg) = &cmd.arg {
                assert!(
                    arg.choices.len() <= 25,
                    "command /{} has {} choices (max 25)",
                    cmd.name,
                    arg.choices.len(),
                );
                for &(label, value) in arg.choices {
                    assert!(
                        label.len() <= 100,
                        "command /{} choice label too long: {label}",
                        cmd.name,
                    );
                    assert!(
                        value.len() <= 100,
                        "command /{} choice value too long: {value}",
                        cmd.name,
                    );
                }
            }
        }
    }
}
