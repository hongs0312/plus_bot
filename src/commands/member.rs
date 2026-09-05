use std::fmt::Write;
use tracing::{error, info};

use serenity::all::*;
use serenity::builder::EditInteractionResponse;
use serenity::model::application::CommandDataOptionValue::{SubCommand, User};

use crate::cache::*;
use crate::commands::utils::*;
use crate::integration::notion::member::NotionMember;
use crate::integration::notion::project::{update_project, Project};

// 명령어 실행 함수
pub async fn run_member_command(
    ctx: &Context,
    command: &CommandInteraction,
) -> serenity::Result<()> {
    // 타임아웃 방지를 위한 상호작용 defer 처리
    command.defer(&ctx.http).await?;

    // 명령어가 입력된 채널 정보 가져오기
    let Some(guild_id) = command.guild_id else {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content("❌ 이 명령어는 서버 안에서만 사용 가능합니다."),
            )
            .await?;
        return Ok(());
    };

    // 전역 공유 캐시 가져오기
    let data_read = ctx.data.read().await;
    let cache_lock = data_read
        .get::<crate::cache::SharedCacheKey>()
        .expect("보관함에 캐시가 없습니다.")
        .clone();

    // 필요한 컨텍스트를 구조체로 묶기
    let channel_manager = ChannelManager {
        ctx,
        guild_id,
        command,
        cache_lock,
    };

    // 프로젝트 카테고리 내부 채널에서만 명령어를 실행할 수 있도록 검증
    let error_message = "❌ 이 명령어는 프로젝트 카테고리 내부 채널에서만 사용 가능합니다.";
    let category_id = match get_project_category_id(&channel_manager, error_message).await {
        Some(id) => id,
        None => return Ok(()),
    };

    // 실제 명령어 처리 함수 호출
    handle_member_command(&channel_manager, category_id).await?;

    Ok(())
}

// 명령을 처리하는 실제 함수
async fn handle_member_command(
    channel_manager: &ChannelManager<'_>,
    category_id: ChannelId,
) -> serenity::Result<()> {
    let (ctx, command, cache_lock) = (
        channel_manager.ctx,
        channel_manager.command,
        &channel_manager.cache_lock,
    );

    // 첫 번째 옵션에서 어떤 서브커맨드가 들어왔는지 확인
    let subcommand_option = match command.data.options.first() {
        Some(opt) => opt,
        None => {
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new().content("⚠️ 올바른 하위 명령어를 선택해주세요."),
                )
                .await?;
            return Ok(());
        }
    };

    // 카테고리 ID를 통해 프로젝트 이름을 가져오기
    let project_name = category_id
        .to_channel(&ctx.http)
        .await?
        .guild()
        .unwrap()
        .name;

    // 프로젝트 인덱스를 통해 프로젝트 정보 가져오기
    let mut my_project = match BotCache::get_project_by_name(cache_lock, &project_name).await {
        Some(project) => project,
        None => return Ok(()),
    };

    // 프로젝트에 참여중인 인원 수집, pm은 가장 앞에 위치
    let pm_discord_id = convert_string_to_discord_id(&my_project.pm.discord_id).unwrap();
    let mut included_mems = vec![pm_discord_id];

    // 프로젝트 참여자 명단을 순회하며 이름을 수집
    for member in &my_project.participants {
        included_mems.push(convert_string_to_discord_id(&member.discord_id).unwrap());
    }

    let subcommand_name = subcommand_option.name.as_str();
    match subcommand_name {
        // --- 1. 참여 인원 조회 (/member list) ---
        "list" => {
            member_list(&channel_manager, &project_name, &included_mems).await?;
        }

        // --- 2. 멤버 추가 / 내보내기 공통 로직 (add / remove) ---
        "add" | "remove" => {
            // 권한 체크 (PM 이거나 관리자 권한을 가진 유저인지 판별)
            if check_pm_or_admin(channel_manager, category_id)
                .await
                .is_err()
            {
                return Ok(());
            }

            // 각 서브커맨드별 처리 함수 호출
            let result = match subcommand_name {
                "add" => {
                    member_add(
                        &channel_manager,
                        subcommand_option,
                        &included_mems,
                        &mut my_project,
                    )
                    .await
                }
                "remove" => {
                    member_remove(
                        &channel_manager,
                        subcommand_option,
                        &included_mems,
                        &mut my_project,
                    )
                    .await
                }
                _ => unreachable!(),
            };

            // 프로젝트 정보 업데이트 및 캐시 갱신
            match result {
                Ok(()) => info!("멤버 {} 서브커맨드 처리 완료.", subcommand_name),
                Err(why) => {
                    error!("멤버 {} 중 오류 발생: {:?}", subcommand_name, why);
                    return Ok(());
                }
            }

            // Notion API를 통해 프로젝트 정보 업데이트
            match update_project(&my_project.id, &my_project).await {
                Ok(_) => info!(
                    "프로젝트 '{}' 정보가 성공적으로 업데이트되었습니다.",
                    my_project.name
                ),
                Err(why) => error!(
                    "프로젝트 '{}' 정보 업데이트 실패: {:?}",
                    my_project.name, why
                ),
            }
        }
        _ => {}
    }

    Ok(())
}

async fn member_list(
    channel_manager: &ChannelManager<'_>,
    project_name: &str,
    included_mems: &Vec<UserId>,
) -> serenity::Result<()> {
    let (ctx, command, cache_lock) = (
        channel_manager.ctx,
        channel_manager.command,
        &channel_manager.cache_lock,
    );

    // 참여 인원 명단을 문자열로 포맷팅
    let mut content = format!("📌 **'{}' 프로젝트 참여 인원 명단**\n", project_name);
    if included_mems.is_empty() {
        content.push_str("현재 등록된 멤버가 없습니다.\n");
    } else {
        for mem in included_mems {
            // content.push_str(&format!("• {}\n", mem)); // 반복문 안에서 format 사용 시 성능 저하 우려
            // write! 매크로는 버퍼 뒤에 바로 문자열을 포매팅해 추가해 성능저하 적음
            match BotCache::get_member_by_user_id(&cache_lock, *mem).await {
                Some(member) => write!(content, "• {}\n", member.name).unwrap(),
                None => error!("❌ 캐시에서 {} 유저를 찾지 못했습니다.", mem),
            };
        }
    }

    command
        .edit_response(&ctx.http, EditInteractionResponse::new().content(content))
        .await?;

    Ok(())
}

async fn member_add(
    channel_manager: &ChannelManager<'_>,
    subcommand_option: &CommandDataOption,
    already_members: &Vec<UserId>,
    my_project: &mut Project,
) -> Result<(), serenity::Error> {
    let (ctx, guild_id, command, cache_lock) = (
        channel_manager.ctx,
        channel_manager.guild_id,
        channel_manager.command,
        &channel_manager.cache_lock,
    );

    // 서버 내에서 대응되는 프로젝트 역할 ID 검색
    let role_id = get_target_role_id(channel_manager, &my_project).await?;

    let mut processed_users = Vec::new();
    for user_id in extract_target_user_ids(subcommand_option) {
        // 이미 존재하는 유저는 건너뛰기
        if already_members.contains(&user_id) {
            continue;
        }

        let member = match BotCache::get_member_by_user_id(cache_lock, user_id).await {
            Some(member) => member,
            None => continue,
        };

        let notion_member = NotionMember {
            name: member.name.clone(),
            discord_id: user_id.to_string(),
            ..Default::default()
        };

        if ctx
            .http
            .add_member_role(guild_id, user_id, role_id, None)
            .await
            .is_ok()
        {
            processed_users.push(user_id);
            my_project.participants.push(notion_member);
        }
    }

    // 변동 사항이 없거나 작업 가능한 대상 유저가 없을 때 처리
    if processed_users.is_empty() {
        return handle_empty_processed_users(channel_manager).await;
    }

    let mentions: Vec<String> = processed_users
        .iter()
        .map(|id| format!("<@{}>", id))
        .collect();

    command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new().content(format!(
                "✅ {} 님이 '{}' 프로젝트 멤버로 추가되었습니다!",
                mentions.join(", "),
                my_project.name
            )),
        )
        .await?;

    Ok(())
}

async fn member_remove(
    channel_manager: &ChannelManager<'_>,
    subcommand_option: &CommandDataOption,
    already_members: &Vec<UserId>,
    my_project: &mut Project,
) -> serenity::Result<(), serenity::Error> {
    let (ctx, guild_id, command) = (
        channel_manager.ctx,
        channel_manager.guild_id,
        channel_manager.command,
    );

    // 서버 내에서 대응되는 프로젝트 역할 ID 검색
    let role_id = get_target_role_id(channel_manager, &my_project).await?;

    let mut processed_users = Vec::new();
    for user_id in extract_target_user_ids(subcommand_option) {
        if !already_members.contains(&user_id) {
            continue;
        }

        if ctx
            .http
            .remove_member_role(guild_id, user_id, role_id, None)
            .await
            .is_ok()
        {
            processed_users.push(user_id);

            // Notion 프로젝트 참여자 명단에서 제거
            my_project
                .participants
                .retain(|member| member.discord_id != user_id.to_string());
        }
    }

    if processed_users.is_empty() {
        return handle_empty_processed_users(channel_manager).await;
    }

    let mentions: Vec<String> = processed_users
        .iter()
        .map(|id| format!("<@{}>", id))
        .collect();

    command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new().content(format!(
                "🗑️ {} 님이 '{}' 프로젝트 멤버에서 제외되었습니다.",
                mentions.join(", "),
                my_project.name
            )),
        )
        .await?;

    Ok(())
}

// -- 헬퍼 함수들 --

// PM 혹은 관리자 권한을 가진 유저인지 판별
async fn check_pm_or_admin(
    channel_manager: &ChannelManager<'_>,
    category_id: ChannelId,
) -> serenity::Result<()> {
    let (ctx, command) = (channel_manager.ctx, channel_manager.command);

    // 권한 체크 (PM 이거나 관리자 권한을 가진 유저인지 판별)
    let is_admin = is_server_admin(&channel_manager).await;
    let is_pm = is_project_pm(&channel_manager, category_id).await;
    if !is_pm && !is_admin {
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content("❌ 이 명령어를 사용할 권한이 없습니다. (PM 혹은 관리자만 가능)"),
            )
            .await?;
        return Err(serenity::Error::Other("권한이 없습니다."));
    }

    Ok(())
}

// 프로젝트에 대응되는 역할 ID를 가져오는 헬퍼 함수
async fn get_target_role_id(
    channel_manager: &ChannelManager<'_>,
    my_project: &Project,
) -> serenity::Result<RoleId, serenity::Error> {
    let (ctx, guild_id, command) = (
        channel_manager.ctx,
        channel_manager.guild_id,
        channel_manager.command,
    );

    // 서버 내에서 대응되는 프로젝트 역할 ID 검색
    let mut target_role_id = None;
    if let Ok(roles) = guild_id.roles(&ctx.http).await {
        if let Some(role) = roles.values().find(|r| r.name == my_project.name) {
            target_role_id = Some(role.id);
        }
    }

    let role_id = match target_role_id {
        Some(id) => id,
        None => {
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new().content(format!(
                        "❌ 서버에서 '{}' 프로젝트에 해당하는 역할을 찾을 수 없습니다.",
                        my_project.name
                    )),
                )
                .await?;
            return Err(serenity::Error::Other("역할을 찾을 수 없습니다."));
        }
    };

    Ok(role_id)
}

// 서브커맨드 멘션 옵션에서 대상 유저 ID들을 추출하는 헬퍼 함수
fn extract_target_user_ids(subcommand_option: &CommandDataOption) -> Vec<UserId> {
    let mut target_user_ids = Vec::new();

    if let SubCommand(sub_opts) = &subcommand_option.value {
        for opt in sub_opts {
            if let User(user_id) = opt.value {
                target_user_ids.push(user_id);
            }
        }
    }

    target_user_ids
}

// 변동 사항이 없거나 작업 가능한 대상 유저가 없을 때 처리하는 헬퍼 함수
async fn handle_empty_processed_users(
    channel_manager: &ChannelManager<'_>,
) -> serenity::Result<(), serenity::Error> {
    let (ctx, command) = (channel_manager.ctx, channel_manager.command);

    command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new()
                .content("❌ 변동 사항이 없거나 작업 가능한 대상 유저가 없습니다."),
        )
        .await?;

    return Err(serenity::Error::Other(
        "변동 사항이 없거나 작업 가능한 대상 유저가 없습니다.",
    ));
}

// 슬래시 커맨드 등록 함수
pub fn register_member_command() -> CreateCommand {
    CreateCommand::new("member")
        .description("프로젝트 멤버를 관리하는 명령어입니다.")
        .dm_permission(false)
        // 1. list 서브커맨드 (/member list)
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "list",
            "현재 프로젝트에 참여 중인 인원 목록을 확인합니다.",
        ))
        // 2. add 서브커맨드 (/member add [user1] [user2] [user3])
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "add",
                "프로젝트에 새 멤버를 추가하고 역할을 부여합니다.",
            )
            .add_sub_option(
                CreateCommandOption::new(CommandOptionType::User, "user1", "추가할 첫 번째 유저")
                    .required(true),
            )
            .add_sub_option(
                CreateCommandOption::new(
                    CommandOptionType::User,
                    "user2",
                    "추가할 두 번째 유저 (선택)",
                )
                .required(false),
            )
            .add_sub_option(
                CreateCommandOption::new(
                    CommandOptionType::User,
                    "user3",
                    "추가할 세 번째 유저 (선택)",
                )
                .required(false),
            ),
        )
        // 3. remove 서브커맨드 (/member remove [user1] [user2] [user3])
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "remove",
                "프로젝트에서 멤버를 내보내고 역할을 회수합니다.",
            )
            .add_sub_option(
                CreateCommandOption::new(CommandOptionType::User, "user1", "내보낼 첫 번째 유저")
                    .required(true),
            )
            .add_sub_option(
                CreateCommandOption::new(
                    CommandOptionType::User,
                    "user2",
                    "내보낼 두 번째 유저 (선택)",
                )
                .required(false),
            )
            .add_sub_option(
                CreateCommandOption::new(
                    CommandOptionType::User,
                    "user3",
                    "내보낼 세 번째 유저 (선택)",
                )
                .required(false),
            ),
        )
}
