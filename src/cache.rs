/*
cache.rs
프로젝트를 저장하는 캐쉬 구조 구현
*/

use std::collections::HashMap;
use std::sync::Arc;

use tracing::{error, info};

use serenity::all::UserId;
use serenity::gateway::ShardManager;
use serenity::prelude::TypeMapKey;

use tokio::sync::{mpsc, RwLock};

use crate::commands::utils::convert_string_to_discord_id;
use crate::integration::notion::{member::*, project::*};

// 캐시 스레드가 처리할 명령 목록
#[derive(Debug)]
// pub enum CacheCommand {
//     RefreshAll,     // 모든 캐시를 갱신
//     RefreshProject, // 프로젝트 캐시만 갱신
//     RefreshMember,  // 멤버 캐시만 갱신
//     // UpdateSingleMember { user_id: UserId, display_name: String },
// }

pub struct SharedCacheKey;

impl TypeMapKey for SharedCacheKey {
    type Value = Arc<RwLock<BotCache>>;
}

// 💡 봇 전체에서 "캐시 갱신 신호"를 보낼 수 있도록 Sender를 전역 키로 등록합니다.
// pub struct CacheNotifyKey;
// impl TypeMapKey for CacheNotifyKey {
//     type Value = mpsc::Sender<CacheCommand>;
// }

pub struct ShardManagerContainer;
impl TypeMapKey for ShardManagerContainer {
    type Value = Arc<ShardManager>;
}

//봇이 전체적으로 공유할 캐쉬 구조체
pub struct BotCache {
    // 유저 아이디로 관리
    pub user_id_to_index: HashMap<UserId, usize>,
    pub all_members: Vec<NotionMember>,

    pub project_name_to_id: HashMap<String, String>, // 프로젝트 이름 -> 프로젝트 노션 아이디
    pub project_id_to_index: HashMap<String, usize>, // 프로젝트 노션 아이디 -> all_projects 인덱스
    pub all_projects: Vec<Project>,
}

impl BotCache {
    pub fn new() -> Self {
        BotCache {
            user_id_to_index: HashMap::new(),
            all_members: Vec::new(),

            project_name_to_id: HashMap::new(),
            project_id_to_index: HashMap::new(),
            all_projects: Vec::new(),
        }
    }

    /// 캐시 미스 시 자동으로 리프레시 후 재조회하는 공통 제네릭 도우미 함수
    async fn get_or_refresh<T, FGet, FRef, Fut>(
        cache_lock: &Arc<RwLock<BotCache>>,
        getter: FGet,
        refresher: FRef,
        key_desc: &str, // 에러 로그용 설명 (예: "유저 ID 1234", "프로젝트 '플러스봇'")
    ) -> Option<T>
    where
        FGet: Fn(&BotCache) -> Option<T>,
        FRef: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), Box<dyn std::error::Error>>>,
    {
        info!("🔍 캐시 조회 시도: [{}]", key_desc);

        // 1. Read Lock으로 캐시 조회
        {
            let cache = cache_lock.read().await;
            if let Some(val) = getter(&cache) {
                return Some(val);
            }
        }

        // 2. 캐시 미스 시 갱신 실행
        info!(
            "❌ 캐시에서 [{}]를 찾지 못했습니다. 캐시를 갱신합니다.",
            key_desc
        );
        let _ = refresher().await;

        // 3. 갱신 후 재조회
        let cache = cache_lock.read().await;
        let res = getter(&cache);
        if res.is_none() {
            error!("❌ 캐시 갱신 후에도 [{}]를 찾지 못했습니다.", key_desc);
        }
        res
    }

    // 유저 아이디로 멤버 정보를 가져오는 함수
    pub async fn get_member_by_user_id(
        cache_lock: &Arc<RwLock<BotCache>>,
        user_id: UserId,
    ) -> Option<NotionMember> {
        Self::get_or_refresh(
            cache_lock,
            |c| {
                c.user_id_to_index
                    .get(&user_id)
                    .and_then(|&idx| c.all_members.get(idx).cloned())
            },
            || Self::refresh_about_member(cache_lock),
            &format!("유저 ID {}", user_id),
        )
        .await
    }

    // 프로젝트 이름으로 프로젝트 id를 가져오는 함수
    pub async fn get_project_id_by_name(
        cache_lock: &Arc<RwLock<BotCache>>,
        project_name: &str,
    ) -> Option<String> {
        Self::get_or_refresh(
            cache_lock,
            |c| c.project_name_to_id.get(project_name).cloned(),
            || Self::refresh_about_project(cache_lock),
            &format!("프로젝트 이름 '{}'", project_name),
        )
        .await
    }

    // 프로젝트 ID로 프로젝트 정보를 가져오는 함수
    pub async fn get_project_by_id(
        cache_lock: &Arc<RwLock<BotCache>>,
        project_id: &str,
    ) -> Option<Project> {
        Self::get_or_refresh(
            cache_lock,
            |c| {
                c.project_id_to_index
                    .get(project_id)
                    .and_then(|&idx| c.all_projects.get(idx).cloned())
            },
            || Self::refresh_about_project(cache_lock),
            &format!("프로젝트 ID '{}'", project_id),
        )
        .await
    }

    // 프로젝트 이름으로 프로젝트 정보를 가져오는 함수
    pub async fn get_project_by_name(
        cache_lock: &Arc<RwLock<BotCache>>,
        project_name: &str,
    ) -> Option<Project> {
        if let Some(project_id) = Self::get_project_id_by_name(cache_lock, project_name).await {
            Self::get_project_by_id(cache_lock, &project_id).await
        } else {
            None
        }
    }

    // 프로젝트 관련 캐시 갱신
    pub async fn refresh_about_project(
        cache_lock: &Arc<RwLock<BotCache>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 노션 API에서 프로젝트 목록 가져오기
        let projects = get_projects().await?;
        let mut cache = cache_lock.write().await;

        // 기존 매핑 초기화
        cache.project_name_to_id.clear();
        cache.project_id_to_index.clear();

        // 프로젝트 이름과 ID를 키로 사용하여 인덱스를 매핑
        for (index, project) in projects.iter().enumerate() {
            cache
                .project_name_to_id
                .insert(project.name.clone(), project.id.clone());
            cache.project_id_to_index.insert(project.id.clone(), index);

            // println!("{:?} 프로젝트 캐시 갱신 완료", project);
        }

        // 모든 프로젝트를 캐시에 저장
        cache.all_projects = projects;

        Ok(())
    }

    // 멤버 관련 캐시 갱신
    pub async fn refresh_about_member(
        cache_lock: &Arc<RwLock<BotCache>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 노션 API에서 멤버 목록 가져오기
        let members = match get_members().await {
            Ok(members) => members,
            Err(e) => {
                error!("멤버 목록을 가져오는 데 실패했습니다: {}", e);
                return Err(e);
            }
        };

        // 캐시 잠금 획득
        let mut cache = cache_lock.write().await;

        // 기존 매핑 초기화
        cache.user_id_to_index.clear();

        // 유저 아이디와 인덱스를 매핑
        for (index, member) in members.iter().enumerate() {
            if let Some(user_id) = convert_string_to_discord_id(&member.discord_id) {
                cache.user_id_to_index.insert(user_id, index);
            } else {
                error!(
                    "멤버 {}의 Discord ID 변환에 실패했습니다: {}",
                    member.name, member.discord_id
                );
            }
        }

        // 모든 멤버를 캐시에 저장
        cache.all_members = members;

        Ok(())
    }

    // pub async fn get_all_projects(cache_lock: &Arc<RwLock<BotCache>>) -> Vec<Project> {
    //     cache_lock.read().await.all_projects.clone()
    // }

    // pub async fn get_all_members(cache_lock: &Arc<RwLock<BotCache>>) -> Vec<NotionMember> {
    //     cache_lock.read().await.all_members.clone()
    // }
}

// // 쓰레드 구성
// pub fn start_cache_thread(
//     cache: Arc<RwLock<BotCache>>,
//     _http: Arc<Http>,
//     _guild_id: GuildId,
// ) -> mpsc::Sender<CacheCommand> {
//     // 버퍼 크기가 32인 비동기 채널 생성(가동신호 수신용)
//     let (tx, mut rx) = mpsc::channel::<CacheCommand>(32);

//     tokio::spawn(async move {
//         info!("백그라운드 동기화 스레드 가동");

//         while let Some(command) = rx.recv().await {
//             match command {
//                 CacheCommand::RefreshAll => {
//                     info!("[캐시] 전체 캐시 즉시 동기화 요청 처리 중...");

//                     BotCache::refresh_about_project(&cache).await;
//                     BotCache::refresh_about_member(&cache).await;

//                     info!("[캐시] 전체 캐시 즉시 동기화 완료");
//                 }
//                 CacheCommand::RefreshProject => {
//                     info!("[캐시] 프로젝트 캐시 즉시 동기화 요청 처리 중...");

//                     BotCache::refresh_about_project(&cache).await;

//                     info!("[캐시] 프로젝트 캐시 즉시 동기화 완료");
//                 }
//                 CacheCommand::RefreshMember => {
//                     info!("[캐시] 멤버 캐시 즉시 동기화 요청 처리 중...");

//                     BotCache::refresh_about_member(&cache).await;

//                     info!("[캐시] 멤버 캐시 즉시 동기화 완료");
//                 }
//             }
//         }
//         // 만약 봇이 꺼지거나 tx를 가진 곳이 전부 드롭되면 루프 종료.
//         info!("백그라운드 동기화 스레드 종료");
//     });

//     tx
// }
