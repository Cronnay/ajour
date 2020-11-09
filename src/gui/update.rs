use {
    super::{
        component, AddonVersionKey, Ajour, CatalogCategory, CatalogColumnKey, CatalogInstallAddon,
        CatalogInstallStatus, CatalogRow, CatalogSource, Changelog, ChangelogPayload, ColumnKey,
        Component, DirectoryType, DownloadReason, ExpandType, Interaction, Message, Mode,
        SelfUpdateStatus, SortDirection, State,
    },
    ajour_core::{
        addon::{Addon, AddonFolder, AddonState, Repository},
        backup::latest_backup,
        cache::{
            remove_addon_cache_entry, update_addon_cache, AddonCache, AddonCacheEntry,
            FingerprintCache,
        },
        catalog,
        config::{ColumnConfig, ColumnConfigV2, Flavor},
        curse_api,
        error::ClientError,
        fs::{delete_addons, install_addon, PersistentData},
        network::download_addon,
        parse::{read_addon_directory, update_addon_fingerprint},
        tukui_api,
        utility::{download_update_to_temp_file, wow_path_resolution},
        wowi_api, Result,
    },
    async_std::sync::{Arc, Mutex},
    iced::{Command, Length},
    isahc::HttpClient,
    native_dialog::*,
    std::collections::{HashMap, HashSet},
    std::convert::TryFrom,
    std::path::{Path, PathBuf},
    widgets::header::ResizeEvent,
};

pub fn handle_message(ajour: &mut Ajour, message: Message) -> Result<Command<Message>> {
    match message {
        Message::Component(component) => match component {
            Component::Settings(message) => return component::settings::update(ajour, message),
        },
        Message::CachesLoaded(result) => {
            log::debug!("Message::CachesLoaded(error: {})", result.is_err());

            if let Ok((fingerprint_cache, addon_cache)) = result {
                ajour.fingerprint_cache = Some(Arc::new(Mutex::new(fingerprint_cache)));
                ajour.addon_cache = Some(Arc::new(Mutex::new(addon_cache)));
            }

            return Ok(Command::perform(async {}, Message::Parse));
        }
        Message::Parse(_) => {
            log::debug!("Message::Parse");

            // Begin to parse addon folder(s).
            let mut commands = vec![];

            // If a backup directory is selected, find the latest backup
            if let Some(dir) = &ajour.config.backup_directory {
                commands.push(Command::perform(
                    latest_backup(dir.to_owned()),
                    Message::LatestBackup,
                ));
            }

            let flavors = &Flavor::ALL[..];
            for flavor in flavors {
                if let Some(addon_directory) = ajour.config.get_addon_directory_for_flavor(flavor) {
                    log::debug!(
                        "preparing to parse addons in {:?}",
                        addon_directory.display()
                    );

                    // Builds a Vec of valid flavors.
                    if addon_directory.exists() {
                        ajour.valid_flavors.push(*flavor);
                        ajour.valid_flavors.dedup();
                    }

                    // Sets loading
                    ajour.state.insert(Mode::MyAddons(*flavor), State::Loading);

                    // Add commands
                    commands.push(Command::perform(
                        perform_read_addon_directory(
                            ajour.addon_cache.clone(),
                            ajour.fingerprint_cache.clone(),
                            addon_directory.clone(),
                            *flavor,
                        ),
                        Message::ParsedAddons,
                    ));
                } else {
                    log::debug!("addon directory is not set, showing welcome screen");

                    // Assume we are welcoming a user because directory is not set.
                    let flavor = ajour.config.wow.flavor;
                    ajour.state.insert(Mode::MyAddons(flavor), State::Start);

                    break;
                }
            }

            let flavor = ajour.config.wow.flavor;
            // If we dont have current flavor in valid flavors we select a new.
            if !ajour.valid_flavors.iter().any(|f| *f == flavor) {
                // Find new flavor.
                if let Some(flavor) = ajour.valid_flavors.first() {
                    // Set nye flavor.
                    ajour.config.wow.flavor = *flavor;
                    // Set mode.
                    ajour.mode = Mode::MyAddons(*flavor);
                    // Persist the newly updated config.
                    ajour.config.save()?;
                }
            }

            return Ok(Command::batch(commands));
        }
        Message::Interaction(Interaction::Refresh) => {
            log::debug!("Interaction::Refresh");

            // Close settings if shown.
            ajour.is_showing_settings = false;
            // Close details if shown.
            ajour.expanded_type = ExpandType::None;

            // Cleans the addons.
            ajour.addons = HashMap::new();

            // Prepare state for loading.
            let flavor = ajour.config.wow.flavor;
            ajour.state.insert(Mode::MyAddons(flavor), State::Loading);

            return Ok(Command::perform(async {}, Message::Parse));
        }
        Message::Interaction(Interaction::Settings) => {
            log::debug!("Interaction::Settings");

            ajour.is_showing_settings = !ajour.is_showing_settings;

            // Remove the expanded addon.
            ajour.expanded_type = ExpandType::None;
        }
        Message::Interaction(Interaction::Ignore(id)) => {
            log::debug!("Interaction::Ignore({})", &id);

            // Close settings if shown.
            ajour.is_showing_settings = false;
            // Close details if shown.
            ajour.expanded_type = ExpandType::None;

            let flavor = ajour.config.wow.flavor;
            let addons = ajour.addons.entry(flavor).or_default();
            let addon = addons.iter_mut().find(|a| a.primary_folder_id == id);

            if let Some(addon) = addon {
                addon.state = AddonState::Ignored;

                // Update the config.
                ajour
                    .config
                    .addons
                    .ignored
                    .entry(flavor)
                    .or_default()
                    .push(addon.primary_folder_id.clone());

                // Persist the newly updated config.
                let _ = &ajour.config.save();
            }
        }
        Message::Interaction(Interaction::Unignore(id)) => {
            log::debug!("Interaction::Unignore({})", &id);

            // Update ajour state.
            let flavor = ajour.config.wow.flavor;
            let addons = ajour.addons.entry(flavor).or_default();
            if let Some(addon) = addons.iter_mut().find(|a| a.primary_folder_id == id) {
                // Check if addon is updatable.
                if let Some(package) = addon.relevant_release_package() {
                    if addon.is_updatable(package) {
                        addon.state = AddonState::Updatable;
                    } else {
                        addon.state = AddonState::Ajour(None);
                    }
                }
            };

            // Update the config.
            let ignored_addon_ids = ajour.config.addons.ignored.entry(flavor).or_default();
            ignored_addon_ids.retain(|i| i != &id);

            // Persist the newly updated config.
            let _ = &ajour.config.save();
        }
        Message::Interaction(Interaction::OpenDirectory(dir_type)) => {
            log::debug!("Interaction::OpenDirectory({:?})", dir_type);

            let message = match dir_type {
                DirectoryType::Wow => Message::UpdateWowDirectory,
                DirectoryType::Backup => Message::UpdateBackupDirectory,
            };

            return Ok(Command::perform(open_directory(), message));
        }
        Message::Interaction(Interaction::OpenLink(link)) => {
            log::debug!("Interaction::OpenLink({})", &link);

            return Ok(Command::perform(
                async {
                    let _ = opener::open(link);
                },
                Message::None,
            ));
        }
        Message::UpdateWowDirectory(chosen_path) => {
            log::debug!("Message::UpdateWowDirectory(Chosen({:?}))", &chosen_path);
            let path = wow_path_resolution(chosen_path);
            log::debug!("Message::UpdateWowDirectory(Resolution({:?}))", &path);

            if path.is_some() {
                // Clear addons.
                ajour.addons = HashMap::new();
                // Update the path for World of Warcraft.
                ajour.config.wow.directory = path;
                // Persist the newly updated config.
                let _ = &ajour.config.save();
                // Set loading state.
                let state = ajour.state.clone();
                for (mode, _) in state {
                    if matches!(mode, Mode::MyAddons(_)) {
                        ajour.state.insert(mode, State::Loading);
                    }
                }

                return Ok(Command::perform(async {}, Message::Parse));
            }
        }
        Message::Interaction(Interaction::FlavorSelected(flavor)) => {
            log::debug!("Interaction::FlavorSelected({})", flavor);
            // Close settings if shown.
            ajour.is_showing_settings = false;
            // Close details if shown.
            ajour.expanded_type = ExpandType::None;
            // Update the game flavor
            ajour.config.wow.flavor = flavor;
            // Persist the newly updated config.
            let _ = &ajour.config.save();
            // Update flavor on MyAddons if thats our current mode.
            if let Mode::MyAddons(_) = ajour.mode {
                ajour.mode = Mode::MyAddons(flavor)
            }
            // Update catalog
            query_and_sort_catalog(ajour);
        }
        Message::Interaction(Interaction::ModeSelected(mode)) => {
            log::debug!("Interaction::ModeSelected({:?})", mode);

            // Close settings if shown.
            ajour.is_showing_settings = false;

            // Sets mode.
            ajour.mode = mode;
        }

        Message::Interaction(Interaction::Expand(expand_type)) => {
            // Close settings if shown.
            ajour.is_showing_settings = false;

            // An addon can be exanded in two ways.
            match &expand_type {
                ExpandType::Details(a) => {
                    log::debug!("Interaction::Expand(Details({:?}))", &a.primary_folder_id);
                    let should_close = match &ajour.expanded_type {
                        ExpandType::Details(ea) => a.primary_folder_id == ea.primary_folder_id,
                        _ => false,
                    };

                    if should_close {
                        ajour.expanded_type = ExpandType::None;
                    } else {
                        ajour.expanded_type = expand_type.clone();
                    }
                }
                ExpandType::Changelog(changelog) => match changelog {
                    // We request changelog.
                    Changelog::Request(addon, key) => {
                        log::debug!(
                            "Interaction::Expand(Changelog::Request({:?}))",
                            &addon.primary_folder_id
                        );

                        // Check if the current expanded_type is showing changelog, and is the same
                        // addon. If this is the case, we close the details.

                        if let ExpandType::Changelog(Changelog::Some(a, _, k)) =
                            &ajour.expanded_type
                        {
                            if addon.primary_folder_id == a.primary_folder_id && key == k {
                                ajour.expanded_type = ExpandType::None;
                                return Ok(Command::none());
                            }
                        }

                        // If we have a curse addon.
                        if addon.active_repository == Some(Repository::Curse) {
                            let file_id = match key {
                                AddonVersionKey::Local => addon.file_id(),
                                AddonVersionKey::Remote => {
                                    if let Some(package) = addon.relevant_release_package() {
                                        package.file_id
                                    } else {
                                        None
                                    }
                                }
                            };

                            if let (Some(id), Some(file_id)) = (addon.repository_id(), file_id) {
                                let id = id.parse::<u32>().unwrap();

                                ajour.expanded_type =
                                    ExpandType::Changelog(Changelog::Loading(addon.clone(), *key));
                                return Ok(Command::perform(
                                    perform_fetch_curse_changelog(addon.clone(), *key, id, file_id),
                                    Message::FetchedChangelog,
                                ));
                            }
                        }

                        // If we have a Tukui addon.
                        if addon.active_repository == Some(Repository::Tukui) {
                            if let Some(id) = addon.repository_id() {
                                ajour.expanded_type =
                                    ExpandType::Changelog(Changelog::Loading(addon.clone(), *key));
                                return Ok(Command::perform(
                                    perform_fetch_tukui_changelog(
                                        addon.clone(),
                                        id,
                                        ajour.config.wow.flavor,
                                        *key,
                                    ),
                                    Message::FetchedChangelog,
                                ));
                            }
                        }

                        // If we have a Wowi addon.
                        if addon.active_repository == Some(Repository::WowI) {
                            if let Some(id) = addon.repository_id() {
                                ajour.expanded_type =
                                    ExpandType::Changelog(Changelog::Loading(addon.clone(), *key));
                                return Ok(Command::perform(
                                    perform_fetch_wowi_changelog(addon.clone(), id, *key),
                                    Message::FetchedChangelog,
                                ));
                            }
                        }
                    }
                    Changelog::Loading(a, _) => {
                        log::debug!(
                            "Interaction::Expand(Changelog::Loading({:?}))",
                            &a.primary_folder_id
                        );
                        ajour.expanded_type = ExpandType::Changelog(changelog.clone());
                    }
                    Changelog::Some(a, _, _) => {
                        log::debug!(
                            "Interaction::Expand(Changelog::Some({:?}))",
                            &a.primary_folder_id
                        );
                    }
                },
                ExpandType::None => {
                    log::debug!("Interaction::Expand(ExpandType::None)");
                }
            }
        }
        Message::Interaction(Interaction::Delete(id)) => {
            log::debug!("Interaction::Delete({})", &id);

            // Close settings if shown.
            ajour.is_showing_settings = false;
            // Close details if shown.
            ajour.expanded_type = ExpandType::None;

            let flavor = ajour.config.wow.flavor;
            let addons = ajour.addons.entry(flavor).or_default();

            if let Some(addon) = addons.iter().find(|a| a.primary_folder_id == id).cloned() {
                // Remove from local state.
                addons.retain(|a| a.primary_folder_id != addon.primary_folder_id);

                // Delete addon(s) from disk.
                let _ = delete_addons(&addon.folders);

                // Remove addon from cache
                if let Some(addon_cache) = &ajour.addon_cache {
                    if let Ok(entry) = AddonCacheEntry::try_from(&addon) {
                        match addon.active_repository {
                            // Delete the entry for this cached addon
                            Some(Repository::Tukui) | Some(Repository::WowI) => {
                                return Ok(Command::perform(
                                    remove_addon_cache_entry(addon_cache.clone(), entry, flavor),
                                    Message::AddonCacheEntryRemoved,
                                ));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Message::Interaction(Interaction::Update(id)) => {
            log::debug!("Interaction::Update({})", &id);

            // Close settings if shown.
            ajour.is_showing_settings = false;
            // Close details if shown.
            ajour.expanded_type = ExpandType::None;

            let flavor = ajour.config.wow.flavor;
            let addons = ajour.addons.entry(flavor).or_default();
            let to_directory = ajour
                .config
                .get_download_directory_for_flavor(flavor)
                .expect("Expected a valid path");
            for addon in addons.iter_mut() {
                if addon.primary_folder_id == id {
                    addon.state = AddonState::Downloading;
                    return Ok(Command::perform(
                        perform_download_addon(
                            DownloadReason::Update,
                            ajour.shared_client.clone(),
                            flavor,
                            addon.clone(),
                            to_directory,
                        ),
                        Message::DownloadedAddon,
                    ));
                }
            }
        }
        Message::Interaction(Interaction::UpdateAll) => {
            log::debug!("Interaction::UpdateAll");

            // Close settings if shown.
            ajour.is_showing_settings = false;
            // Close details if shown.
            ajour.expanded_type = ExpandType::None;

            // Update all updatable addons, expect ignored.
            let flavor = ajour.config.wow.flavor;
            let ignored_ids = ajour.config.addons.ignored.entry(flavor).or_default();
            let mut addons: Vec<_> = ajour
                .addons
                .entry(flavor)
                .or_default()
                .iter_mut()
                .filter(|a| !ignored_ids.iter().any(|i| i == &a.primary_folder_id))
                .collect();

            let mut commands = vec![];
            for addon in addons.iter_mut() {
                if addon.state == AddonState::Updatable {
                    if let Some(to_directory) =
                        ajour.config.get_download_directory_for_flavor(flavor)
                    {
                        addon.state = AddonState::Downloading;
                        let addon = addon.clone();
                        commands.push(Command::perform(
                            perform_download_addon(
                                DownloadReason::Update,
                                ajour.shared_client.clone(),
                                flavor,
                                addon,
                                to_directory,
                            ),
                            Message::DownloadedAddon,
                        ))
                    }
                }
            }
            return Ok(Command::batch(commands));
        }
        Message::ParsedAddons((flavor, result)) => {
            // if our selected flavor returns (either ok or error) - we change to idle.
            ajour.state.insert(Mode::MyAddons(flavor), State::Ready);

            if let Ok(addons) = result {
                log::debug!("Message::ParsedAddons({}, {} addons)", flavor, addons.len(),);

                // Ignored addon ids.
                let ignored_ids = ajour.config.addons.ignored.entry(flavor).or_default();

                // Check if addons is updatable.
                let release_channels = ajour
                    .config
                    .addons
                    .release_channels
                    .entry(flavor)
                    .or_default();
                let mut addons = addons
                    .into_iter()
                    .map(|mut a| {
                        // Check if we have saved release channel for addon.
                        if let Some(release_channel) = release_channels.get(&a.primary_folder_id) {
                            a.release_channel = *release_channel;
                        } else {
                            // Else we try to determine the release_channel based of installed version.
                            for (release_channel, package) in a.remote_packages() {
                                if package.file_id == a.file_id() {
                                    a.release_channel = release_channel.to_owned();
                                    break;
                                }
                            }
                        }

                        // Check if addon is updatable based on release channel.
                        if let Some(package) = a.relevant_release_package() {
                            if a.is_updatable(package) && a.state != AddonState::Corrupted {
                                a.state = AddonState::Updatable;
                            }
                        }

                        if ignored_ids.iter().any(|ia| &a.primary_folder_id == ia) {
                            a.state = AddonState::Ignored;
                        };

                        a
                    })
                    .collect::<Vec<Addon>>();

                // Sort the addons.
                sort_addons(&mut addons, SortDirection::Desc, ColumnKey::Status);
                ajour.header_state.previous_sort_direction = Some(SortDirection::Desc);
                ajour.header_state.previous_column_key = Some(ColumnKey::Status);

                // Sets the flavor state to ready.
                ajour.state.insert(Mode::MyAddons(flavor), State::Ready);

                // Insert the addons into the HashMap.
                ajour.addons.insert(flavor, addons);
            } else {
                log::error!(
                    "Message::ParsedAddons({}) - {}",
                    flavor,
                    result.err().unwrap(),
                );
            }
        }
        Message::DownloadedAddon((reason, flavor, id, result)) => {
            log::debug!(
                "Message::DownloadedAddon(({}, {}, error: {}))",
                flavor,
                &id,
                result.is_err()
            );

            let addons = ajour.addons.entry(flavor).or_default();
            let install_addons = ajour.catalog_install_addons.entry(flavor).or_default();

            let mut addon = None;

            match result {
                Ok(_) => match reason {
                    DownloadReason::Update => {
                        if let Some(_addon) = addons.iter_mut().find(|a| a.primary_folder_id == id)
                        {
                            addon = Some(_addon);
                        }
                    }
                    DownloadReason::Install => {
                        if let Some(install_addon) = install_addons
                            .iter_mut()
                            .find(|a| a.addon.as_ref().map(|a| &a.primary_folder_id) == Some(&id))
                        {
                            install_addon.status = CatalogInstallStatus::Unpacking;

                            if let Some(_addon) = install_addon.addon.as_mut() {
                                addon = Some(_addon);
                            }
                        }
                    }
                },
                Err(error) => {
                    log::error!("{}", error);
                    ajour.error = Some(error.to_string());

                    if reason == DownloadReason::Install {
                        if let Some(install_addon) =
                            install_addons.iter_mut().find(|a| a.id.to_string() == id)
                        {
                            install_addon.status = CatalogInstallStatus::Retry;
                        }
                    }
                }
            }

            if let Some(addon) = addon {
                let from_directory = ajour
                    .config
                    .get_download_directory_for_flavor(flavor)
                    .expect("Expected a valid path");
                let to_directory = ajour
                    .config
                    .get_addon_directory_for_flavor(&flavor)
                    .expect("Expected a valid path");

                if addon.state == AddonState::Downloading {
                    addon.state = AddonState::Unpacking;

                    return Ok(Command::perform(
                        perform_unpack_addon(
                            reason,
                            flavor,
                            addon.clone(),
                            from_directory,
                            to_directory,
                        ),
                        Message::UnpackedAddon,
                    ));
                }
            }
        }
        Message::UnpackedAddon((reason, flavor, id, result)) => {
            log::debug!(
                "Message::UnpackedAddon(({}, error: {}))",
                &id,
                result.is_err()
            );

            let addons = ajour.addons.entry(flavor).or_default();
            let install_addons = ajour.catalog_install_addons.entry(flavor).or_default();

            let mut addon = None;
            let mut folders = None;

            match result {
                Ok(_folders) => match reason {
                    DownloadReason::Update => {
                        if let Some(_addon) = addons.iter_mut().find(|a| a.primary_folder_id == id)
                        {
                            addon = Some(_addon);
                            folders = Some(_folders);
                        }
                    }
                    DownloadReason::Install => {
                        if let Some(install_addon) = install_addons
                            .iter_mut()
                            .find(|a| a.addon.as_ref().map(|a| &a.primary_folder_id) == Some(&id))
                        {
                            if let Some(_addon) = install_addon.addon.as_mut() {
                                // If we are installing from the catalog, remove any existing addon
                                // that has the same folders and insert this new one
                                addons.retain(|a| a.folders != _folders);
                                addons.push(_addon.clone());

                                addon = addons.iter_mut().find(|a| a.primary_folder_id == id);
                                folders = Some(_folders);
                            }
                        }

                        // Remove install addon since we've successfully installed it and
                        // added to main addon vec
                        install_addons.retain(|a| {
                            a.addon.as_ref().map(|a| &a.primary_folder_id) != Some(&id)
                        });
                    }
                },
                Err(error) => {
                    log::error!("{}", error);
                    ajour.error = Some(error.to_string());

                    if reason == DownloadReason::Install {
                        if let Some(install_addon) =
                            install_addons.iter_mut().find(|a| a.id.to_string() == id)
                        {
                            install_addon.status = CatalogInstallStatus::Retry;
                        }
                    }
                }
            }

            let mut commands = vec![];

            if let (Some(addon), Some(folders)) = (addon, folders) {
                addon.update_addon_folders(&folders);

                addon.state = AddonState::Fingerprint;

                let mut version = None;
                if let Some(package) = addon.relevant_release_package() {
                    version = Some(package.version.clone());
                }
                if let Some(version) = version {
                    addon.set_version(version);
                }

                // If we are updating / installing a Tukui / WowI
                // addon, we want to update the cache. If we are installing a Curse
                // addon, we want to make sure cache entry exists for those folders
                if let Some(addon_cache) = &ajour.addon_cache {
                    if let Ok(entry) = AddonCacheEntry::try_from(addon as &_) {
                        match addon.active_repository {
                            // Remove any entry related to this cached addon
                            Some(Repository::Curse) => {
                                commands.push(Command::perform(
                                    remove_addon_cache_entry(addon_cache.clone(), entry, flavor),
                                    Message::AddonCacheEntryRemoved,
                                ));
                            }
                            // Update the entry for this cached addon
                            Some(Repository::Tukui) | Some(Repository::WowI) => {
                                commands.push(Command::perform(
                                    update_addon_cache(addon_cache.clone(), entry, flavor),
                                    Message::AddonCacheUpdated,
                                ));
                            }
                            None => {}
                        }
                    }
                }

                // Submit all addon folders to be fingerprinted
                if let Some(cache) = ajour.fingerprint_cache.as_ref() {
                    for folder in &addon.folders {
                        commands.push(Command::perform(
                            perform_hash_addon(
                                ajour
                                    .config
                                    .get_addon_directory_for_flavor(&flavor)
                                    .expect("Expected a valid path"),
                                folder.id.clone(),
                                cache.clone(),
                                flavor,
                            ),
                            Message::UpdateFingerprint,
                        ));
                    }
                }
            }

            if !commands.is_empty() {
                return Ok(Command::batch(commands));
            }
        }
        Message::UpdateFingerprint((flavor, id, result)) => {
            log::debug!(
                "Message::UpdateFingerprint(({:?}, {}, error: {}))",
                flavor,
                &id,
                result.is_err()
            );

            let addons = ajour.addons.entry(flavor).or_default();
            if let Some(addon) = addons.iter_mut().find(|a| a.primary_folder_id == id) {
                if result.is_ok() {
                    addon.state = AddonState::Ajour(Some("Completed".to_owned()));
                } else {
                    addon.state = AddonState::Ajour(Some("Error".to_owned()));
                }
            }
        }
        Message::LatestRelease(release) => {
            log::debug!(
                "Message::LatestRelease({:?})",
                release.as_ref().map(|r| &r.tag_name)
            );

            ajour.self_update_state.latest_release = release;
        }
        Message::Interaction(Interaction::SortColumn(column_key)) => {
            // Close settings if shown.
            ajour.is_showing_settings = false;
            // Close details if shown.
            ajour.expanded_type = ExpandType::None;

            // First time clicking a column should sort it in Ascending order, otherwise
            // flip the sort direction.
            let mut sort_direction = SortDirection::Asc;

            if let Some(previous_column_key) = ajour.header_state.previous_column_key {
                if column_key == previous_column_key {
                    if let Some(previous_sort_direction) =
                        ajour.header_state.previous_sort_direction
                    {
                        sort_direction = previous_sort_direction.toggle()
                    }
                }
            }

            // Exception would be first time ever sorting and sorting by title.
            // Since its already sorting in Asc by default, we should sort Desc.
            if ajour.header_state.previous_column_key.is_none() && column_key == ColumnKey::Title {
                sort_direction = SortDirection::Desc;
            }

            log::debug!(
                "Interaction::SortColumn({:?}, {:?})",
                column_key,
                sort_direction
            );

            let flavor = ajour.config.wow.flavor;
            let mut addons = ajour.addons.entry(flavor).or_default();

            sort_addons(&mut addons, sort_direction, column_key);

            ajour.header_state.previous_sort_direction = Some(sort_direction);
            ajour.header_state.previous_column_key = Some(column_key);
        }
        Message::Interaction(Interaction::SortCatalogColumn(column_key)) => {
            // Close settings if shown.
            ajour.is_showing_settings = false;

            // First time clicking a column should sort it in Ascending order, otherwise
            // flip the sort direction.
            let mut sort_direction = SortDirection::Asc;

            if let Some(previous_column_key) = ajour.catalog_header_state.previous_column_key {
                if column_key == previous_column_key {
                    if let Some(previous_sort_direction) =
                        ajour.catalog_header_state.previous_sort_direction
                    {
                        sort_direction = previous_sort_direction.toggle()
                    }
                }
            }

            // Exception would be first time ever sorting and sorting by title.
            // Since its already sorting in Asc by default, we should sort Desc.
            if ajour.catalog_header_state.previous_column_key.is_none()
                && column_key == CatalogColumnKey::Title
            {
                sort_direction = SortDirection::Desc;
            }

            log::debug!(
                "Interaction::SortCatalogColumn({:?}, {:?})",
                column_key,
                sort_direction
            );

            ajour.catalog_header_state.previous_sort_direction = Some(sort_direction);
            ajour.catalog_header_state.previous_column_key = Some(column_key);

            query_and_sort_catalog(ajour);
        }
        Message::ReleaseChannelSelected(release_channel) => {
            log::debug!("Message::ReleaseChannelSelected({:?})", release_channel);

            if let ExpandType::Details(expanded_addon) = &ajour.expanded_type {
                let flavor = ajour.config.wow.flavor;
                let addons = ajour.addons.entry(flavor).or_default();
                if let Some(addon) = addons
                    .iter_mut()
                    .find(|a| a.primary_folder_id == expanded_addon.primary_folder_id)
                {
                    addon.release_channel = release_channel;

                    // Check if addon is updatable.
                    if let Some(package) = addon.relevant_release_package() {
                        if addon.is_updatable(package) {
                            addon.state = AddonState::Updatable;
                        } else {
                            addon.state = AddonState::Ajour(None);
                        }
                    }

                    // Update config with the newly changed release channel.
                    ajour
                        .config
                        .addons
                        .release_channels
                        .entry(flavor)
                        .or_default()
                        .insert(addon.primary_folder_id.clone(), release_channel);

                    // Persist the newly updated config.
                    let _ = &ajour.config.save();
                }
            }
        }
        Message::ThemesLoaded(mut themes) => {
            log::debug!("Message::ThemesLoaded({} themes)", themes.len());

            themes.sort();

            for theme in themes {
                ajour.theme_state.themes.push((theme.name.clone(), theme));
            }
        }
        Message::Interaction(Interaction::ResizeColumn(column_type, event)) => match event {
            ResizeEvent::ResizeColumn {
                left_name,
                left_width,
                right_name,
                right_width,
            } => match column_type {
                Mode::MyAddons(_) => {
                    let left_key = ColumnKey::from(left_name.as_str());
                    let right_key = ColumnKey::from(right_name.as_str());

                    if let Some(column) = ajour
                        .header_state
                        .columns
                        .iter_mut()
                        .find(|c| c.key == left_key && left_key != ColumnKey::Title)
                    {
                        column.width = Length::Units(left_width);
                    }

                    if let Some(column) = ajour
                        .header_state
                        .columns
                        .iter_mut()
                        .find(|c| c.key == right_key && right_key != ColumnKey::Title)
                    {
                        column.width = Length::Units(right_width);
                    }
                }
                Mode::Catalog => {
                    let left_key = CatalogColumnKey::from(left_name.as_str());
                    let right_key = CatalogColumnKey::from(right_name.as_str());

                    if let Some(column) = ajour
                        .catalog_header_state
                        .columns
                        .iter_mut()
                        .find(|c| c.key == left_key && left_key != CatalogColumnKey::Title)
                    {
                        column.width = Length::Units(left_width);
                    }

                    if let Some(column) = ajour
                        .catalog_header_state
                        .columns
                        .iter_mut()
                        .find(|c| c.key == right_key && right_key != CatalogColumnKey::Title)
                    {
                        column.width = Length::Units(right_width);
                    }
                }
            },
            ResizeEvent::Finished => {
                // Persist changes to config
                save_column_configs(ajour);
            }
        },
        Message::UpdateBackupDirectory(path) => {
            log::debug!("Message::UpdateBackupDirectory({:?})", &path);

            if let Some(path) = path {
                // Update the backup directory path.
                ajour.config.backup_directory = Some(path.clone());
                // Persist the newly updated config.
                let _ = &ajour.config.save();

                // Check if a latest backup exists in path
                return Ok(Command::perform(latest_backup(path), Message::LatestBackup));
            }
        }
        Message::LatestBackup(as_of) => {
            log::debug!("Message::LatestBackup({:?})", &as_of);

            ajour.backup_state.last_backup = as_of;
        }
        Message::BackupFinished(Ok(as_of)) => {
            log::debug!("Message::BackupFinished({})", as_of.format("%H:%M:%S"));

            ajour.backup_state.backing_up = false;
            ajour.backup_state.last_backup = Some(as_of);
        }
        Message::BackupFinished(Err(error)) => {
            log::error!("{}", error);

            ajour.backup_state.backing_up = false;
            ajour.error = Some(error.to_string())
        }
        Message::CatalogDownloaded(Ok(catalog)) => {
            log::debug!(
                "Message::CatalogDownloaded({} addons in catalog)",
                catalog.addons.len()
            );

            let mut categories = HashSet::new();
            catalog.addons.iter().for_each(|a| {
                for category in &a.categories {
                    categories.insert(category.clone());
                }
            });

            // Map category strings to Category enum
            let mut categories: Vec<_> = categories
                .into_iter()
                .map(CatalogCategory::Choice)
                .collect();
            categories.sort();

            // Unshift the All Categories option into the vec
            categories.insert(0, CatalogCategory::All);

            ajour.catalog_search_state.categories = categories;

            ajour.catalog = Some(catalog);

            ajour.state.insert(Mode::Catalog, State::Ready);

            query_and_sort_catalog(ajour);
        }
        Message::Interaction(Interaction::CatalogQuery(query)) => {
            // Close settings if shown.
            ajour.is_showing_settings = false;

            // Catalog search query
            ajour.catalog_search_state.query = Some(query);

            query_and_sort_catalog(ajour);
        }
        Message::Interaction(Interaction::CatalogInstall(source, flavor, id)) => {
            log::debug!(
                "Interaction::CatalogInstall({}, {}, {})",
                source,
                flavor,
                &id
            );

            // Close settings if shown.
            ajour.is_showing_settings = false;

            let install_addons = ajour.catalog_install_addons.entry(flavor).or_default();

            // Remove any existing status for this addon since we are going
            // to try and download it again
            install_addons.retain(|a| id != a.id);

            // Add new status for this addon as Downloading
            install_addons.push(CatalogInstallAddon {
                id,
                status: CatalogInstallStatus::Downloading,
                addon: None,
            });

            return Ok(Command::perform(
                perform_fetch_latest_addon(source, id, flavor),
                Message::CatalogInstallAddonFetched,
            ));
        }
        Message::Interaction(Interaction::CatalogCategorySelected(category)) => {
            log::debug!("Interaction::CatalogCategorySelected({})", &category);
            // Close settings if shown.
            ajour.is_showing_settings = false;

            // Select category
            ajour.catalog_search_state.category = category;

            query_and_sort_catalog(ajour);
        }
        Message::Interaction(Interaction::CatalogResultSizeSelected(size)) => {
            log::debug!("Interaction::CatalogResultSizeSelected({:?})", &size);

            // Close settings if shown.
            ajour.is_showing_settings = false;
            // Catalog result size
            ajour.catalog_search_state.result_size = size;

            query_and_sort_catalog(ajour);
        }
        Message::Interaction(Interaction::CatalogSourceSelected(source)) => {
            log::debug!("Interaction::CatalogResultSizeSelected({:?})", source);

            // Close settings if shown.
            ajour.is_showing_settings = false;
            // Catalog source
            ajour.catalog_search_state.source = source;

            query_and_sort_catalog(ajour);
        }
        Message::CatalogInstallAddonFetched((flavor, id, result)) => {
            let install_addons = ajour.catalog_install_addons.entry(flavor).or_default();

            if let Some(install_addon) = install_addons.iter_mut().find(|a| a.id == id) {
                match result {
                    Ok(mut addon) => {
                        log::debug!(
                            "Message::CatalogInstallAddonFetched({:?}, {:?})",
                            flavor,
                            &id,
                        );

                        addon.state = AddonState::Downloading;
                        install_addon.addon = Some(addon.clone());

                        let to_directory = ajour
                            .config
                            .get_download_directory_for_flavor(flavor)
                            .expect("Expected a valid path");

                        return Ok(Command::perform(
                            perform_download_addon(
                                DownloadReason::Install,
                                ajour.shared_client.clone(),
                                flavor,
                                addon,
                                to_directory,
                            ),
                            Message::DownloadedAddon,
                        ));
                    }
                    Err(error) => {
                        log::error!("{}", error);

                        install_addon.status = CatalogInstallStatus::Unavilable;
                    }
                }
            }
        }
        Message::FetchedChangelog((addon, key, result)) => {
            log::debug!("Message::FetchedChangelog(error: {})", &result.is_err());
            match result {
                Ok((changelog, url)) => {
                    let payload = ChangelogPayload { changelog, url };
                    let changelog = Changelog::Some(addon, payload, key);
                    ajour.expanded_type = ExpandType::Changelog(changelog);
                }
                Err(error) => {
                    log::error!("Message::FetchedChangelog(error: {})", &error);
                    ajour.expanded_type = ExpandType::None;
                }
            }
        }
        Message::Interaction(Interaction::UpdateAjour) => {
            log::debug!("Interaction::UpdateAjour");

            if let Some(release) = &ajour.self_update_state.latest_release {
                let bin_name = bin_name().to_owned();

                ajour.self_update_state.status = Some(SelfUpdateStatus::InProgress);

                return Ok(Command::perform(
                    download_update_to_temp_file(bin_name, release.clone()),
                    Message::AjourUpdateDownloaded,
                ));
            }
        }
        Message::AjourUpdateDownloaded(result) => {
            log::debug!("Message::AjourUpdateDownloaded");

            match result {
                Ok((current_bin_name, temp_bin_path)) => {
                    // Remove first arg, which is path to binary. We don't use this first
                    // arg as binary path because it's not reliable, per the docs.
                    let mut args = std::env::args();
                    args.next();

                    match std::process::Command::new(&temp_bin_path)
                        .args(args)
                        .arg("--self-update-temp")
                        .arg(&current_bin_name)
                        .spawn()
                    {
                        Ok(_) => std::process::exit(0),
                        Err(error) => {
                            log::error!("{}", error);
                            ajour.error = Some(ClientError::from(error).to_string());
                            ajour.self_update_state.status = Some(SelfUpdateStatus::Failed);
                        }
                    }
                }
                Err(error) => {
                    log::error!("{}", error);
                    ajour.error = Some(error.to_string());
                    ajour.self_update_state.status = Some(SelfUpdateStatus::Failed);
                }
            }
        }
        Message::AddonCacheUpdated(Ok(entry)) => {
            log::debug!("Message::AddonCacheUpdated({})", entry.title);
        }
        Message::AddonCacheEntryRemoved(maybe_entry) => {
            if let Some(entry) = maybe_entry {
                log::debug!("Message::AddonCacheEntryRemoved({})", entry.title);
            }
        }
        Message::Error(error)
        | Message::CatalogDownloaded(Err(error))
        | Message::AddonCacheUpdated(Err(error)) => {
            log::error!("{}", error);
            ajour.error = Some(error.to_string());
        }
        Message::RuntimeEvent(iced_native::Event::Window(
            iced_native::window::Event::Resized { width, height },
        )) => {
            let width = (width as f64 * ajour.scale_state.scale) as u32;
            let height = (height as f64 * ajour.scale_state.scale) as u32;

            ajour.config.window_size = Some((width, height));
            let _ = ajour.config.save();
        }
        Message::RuntimeEvent(_) => {}
        Message::None(_) => {}
    }

    Ok(Command::none())
}

async fn open_directory() -> Option<PathBuf> {
    let dialog = OpenSingleDir { dir: None };
    if let Ok(show) = dialog.show() {
        return show;
    }

    None
}

async fn perform_read_addon_directory(
    addon_cache: Option<Arc<Mutex<AddonCache>>>,
    fingerprint_cache: Option<Arc<Mutex<FingerprintCache>>>,
    root_dir: PathBuf,
    flavor: Flavor,
) -> (Flavor, Result<Vec<Addon>>) {
    (
        flavor,
        read_addon_directory(addon_cache, fingerprint_cache, root_dir, flavor).await,
    )
}

async fn perform_fetch_tukui_changelog(
    addon: Addon,
    tukui_id: String,
    flavor: Flavor,
    key: AddonVersionKey,
) -> (Addon, AddonVersionKey, Result<(String, String)>) {
    (
        addon,
        key,
        tukui_api::fetch_changelog(&tukui_id, &flavor).await,
    )
}

async fn perform_fetch_curse_changelog(
    addon: Addon,
    key: AddonVersionKey,
    id: u32,
    file_id: i64,
) -> (Addon, AddonVersionKey, Result<(String, String)>) {
    (addon, key, curse_api::fetch_changelog(id, file_id).await)
}

async fn perform_fetch_wowi_changelog(
    addon: Addon,
    id: String,
    key: AddonVersionKey,
) -> (Addon, AddonVersionKey, Result<(String, String)>) {
    (
        addon,
        key,
        Ok((
            "Please view this changelog in the browser by pressing 'Full Changelog' to the right"
                .to_owned(),
            wowi_api::changelog_url(id),
        )),
    )
}

/// Downloads the newest version of the addon.
/// This is for now only downloading from warcraftinterface.
async fn perform_download_addon(
    reason: DownloadReason,
    shared_client: Arc<HttpClient>,
    flavor: Flavor,
    addon: Addon,
    to_directory: PathBuf,
) -> (DownloadReason, Flavor, String, Result<()>) {
    (
        reason,
        flavor,
        addon.primary_folder_id.clone(),
        download_addon(&shared_client, &addon, &to_directory).await,
    )
}

/// Rehashes a `Addon`.
async fn perform_hash_addon(
    addon_dir: impl AsRef<Path>,
    addon_id: String,
    fingerprint_cache: Arc<Mutex<FingerprintCache>>,
    flavor: Flavor,
) -> (Flavor, String, Result<()>) {
    (
        flavor,
        addon_id.clone(),
        update_addon_fingerprint(fingerprint_cache, flavor, addon_dir, addon_id).await,
    )
}

/// Unzips `Addon` at given `from_directory` and moves it `to_directory`.
async fn perform_unpack_addon(
    reason: DownloadReason,
    flavor: Flavor,
    addon: Addon,
    from_directory: PathBuf,
    to_directory: PathBuf,
) -> (DownloadReason, Flavor, String, Result<Vec<AddonFolder>>) {
    (
        reason,
        flavor,
        addon.primary_folder_id.clone(),
        install_addon(&addon, &from_directory, &to_directory).await,
    )
}

/// Unzips `Addon` at given `from_directory` and moves it `to_directory`.
async fn perform_fetch_latest_addon(
    source: catalog::Source,
    source_id: i32,
    flavor: Flavor,
) -> (Flavor, i32, Result<Addon>) {
    let result = match source {
        catalog::Source::Curse => curse_api::latest_addon(source_id, flavor).await,
        catalog::Source::Tukui => tukui_api::latest_addon(source_id, flavor).await,
        catalog::Source::WowI => wowi_api::latest_addon(source_id, flavor).await,
    };

    (flavor, source_id, result)
}

fn sort_addons(addons: &mut [Addon], sort_direction: SortDirection, column_key: ColumnKey) {
    match (column_key, sort_direction) {
        (ColumnKey::Title, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.title().to_lowercase().cmp(&b.title().to_lowercase()));
        }
        (ColumnKey::Title, SortDirection::Desc) => {
            addons.sort_by(|a, b| {
                a.title()
                    .to_lowercase()
                    .cmp(&b.title().to_lowercase())
                    .reverse()
                    .then_with(|| {
                        a.relevant_release_package()
                            .cmp(&b.relevant_release_package())
                    })
            });
        }
        (ColumnKey::LocalVersion, SortDirection::Asc) => {
            addons.sort_by(|a, b| {
                a.version()
                    .cmp(&b.version())
                    .then_with(|| a.title().cmp(&b.title()))
            });
        }
        (ColumnKey::LocalVersion, SortDirection::Desc) => {
            addons.sort_by(|a, b| {
                a.version()
                    .cmp(&b.version())
                    .reverse()
                    .then_with(|| a.title().cmp(&b.title()))
            });
        }
        (ColumnKey::RemoteVersion, SortDirection::Asc) => {
            addons.sort_by(|a, b| {
                a.relevant_release_package()
                    .cmp(&b.relevant_release_package())
                    .then_with(|| a.cmp(&b))
            });
        }
        (ColumnKey::RemoteVersion, SortDirection::Desc) => {
            addons.sort_by(|a, b| {
                a.relevant_release_package()
                    .cmp(&b.relevant_release_package())
                    .reverse()
                    .then_with(|| a.cmp(&b))
            });
        }
        (ColumnKey::Status, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.state.cmp(&b.state).then_with(|| a.cmp(&b)));
        }
        (ColumnKey::Status, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.state.cmp(&b.state).reverse().then_with(|| a.cmp(&b)));
        }
        (ColumnKey::Channel, SortDirection::Asc) => addons.sort_by(|a, b| {
            a.release_channel
                .to_string()
                .cmp(&b.release_channel.to_string())
        }),
        (ColumnKey::Channel, SortDirection::Desc) => addons.sort_by(|a, b| {
            a.release_channel
                .to_string()
                .cmp(&b.release_channel.to_string())
                .reverse()
        }),
        (ColumnKey::Author, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.author().cmp(&b.author()))
        }
        (ColumnKey::Author, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.author().cmp(&b.author()).reverse())
        }
        (ColumnKey::GameVersion, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.game_version().cmp(&b.game_version()))
        }
        (ColumnKey::GameVersion, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.game_version().cmp(&b.game_version()).reverse())
        }
        (ColumnKey::DateReleased, SortDirection::Asc) => {
            addons.sort_by(|a, b| {
                a.relevant_release_package()
                    .map(|p| p.date_time)
                    .cmp(&b.relevant_release_package().map(|p| p.date_time))
            });
        }
        (ColumnKey::DateReleased, SortDirection::Desc) => {
            addons.sort_by(|a, b| {
                a.relevant_release_package()
                    .map(|p| p.date_time)
                    .cmp(&b.relevant_release_package().map(|p| p.date_time))
                    .reverse()
            });
        }
        (ColumnKey::Source, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.active_repository.cmp(&b.active_repository))
        }
        (ColumnKey::Source, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.active_repository.cmp(&b.active_repository).reverse())
        }
    }
}

fn sort_catalog_addons(
    addons: &mut [CatalogRow],
    sort_direction: SortDirection,
    column_key: CatalogColumnKey,
    flavor: &Flavor,
) {
    match (column_key, sort_direction) {
        (CatalogColumnKey::Title, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.addon.name.cmp(&b.addon.name));
        }
        (CatalogColumnKey::Title, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.addon.name.cmp(&b.addon.name).reverse());
        }
        (CatalogColumnKey::Description, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.addon.summary.cmp(&b.addon.summary));
        }
        (CatalogColumnKey::Description, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.addon.summary.cmp(&b.addon.summary).reverse());
        }
        (CatalogColumnKey::Source, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.addon.source.cmp(&b.addon.source));
        }
        (CatalogColumnKey::Source, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.addon.source.cmp(&b.addon.source).reverse());
        }
        (CatalogColumnKey::NumDownloads, SortDirection::Asc) => {
            addons.sort_by(|a, b| {
                a.addon
                    .number_of_downloads
                    .cmp(&b.addon.number_of_downloads)
            });
        }
        (CatalogColumnKey::NumDownloads, SortDirection::Desc) => {
            addons.sort_by(|a, b| {
                a.addon
                    .number_of_downloads
                    .cmp(&b.addon.number_of_downloads)
                    .reverse()
            });
        }
        (CatalogColumnKey::Install, SortDirection::Asc) => {}
        (CatalogColumnKey::Install, SortDirection::Desc) => {}
        (CatalogColumnKey::DateReleased, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.addon.date_released.cmp(&b.addon.date_released));
        }
        (CatalogColumnKey::DateReleased, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.addon.date_released.cmp(&b.addon.date_released).reverse());
        }
        (CatalogColumnKey::GameVersion, SortDirection::Asc) => addons.sort_by(|a, b| {
            let gv_a = a.addon.game_versions.iter().find(|gc| &gc.flavor == flavor);
            let gv_b = b.addon.game_versions.iter().find(|gc| &gc.flavor == flavor);
            gv_a.cmp(&gv_b)
        }),
        (CatalogColumnKey::GameVersion, SortDirection::Desc) => addons.sort_by(|a, b| {
            let gv_a = a.addon.game_versions.iter().find(|gc| &gc.flavor == flavor);
            let gv_b = b.addon.game_versions.iter().find(|gc| &gc.flavor == flavor);
            gv_a.cmp(&gv_b).reverse()
        }),
    }
}

fn query_and_sort_catalog(ajour: &mut Ajour) {
    if let Some(catalog) = &ajour.catalog {
        let query = ajour
            .catalog_search_state
            .query
            .as_ref()
            .map(|s| s.to_lowercase());
        let flavor = &ajour.config.wow.flavor;
        let source = &ajour.catalog_search_state.source;
        let category = &ajour.catalog_search_state.category;
        let result_size = ajour.catalog_search_state.result_size.as_usize();

        let mut catalog_rows: Vec<_> = catalog
            .addons
            .iter()
            .filter(|a| !a.game_versions.is_empty())
            .filter(|a| {
                let cleaned_text =
                    format!("{} {}", a.name.to_lowercase(), a.summary.to_lowercase());

                if let Some(query) = &query {
                    cleaned_text.contains(query)
                } else {
                    true
                }
            })
            .filter(|a| {
                a.game_versions
                    .iter()
                    .any(|gc| gc.flavor == flavor.base_flavor())
            })
            .filter(|a| match source {
                CatalogSource::Choice(source) => a.source == *source,
            })
            .filter(|a| match category {
                CatalogCategory::All => true,
                CatalogCategory::Choice(name) => a.categories.iter().any(|c| c == name),
            })
            .cloned()
            .map(CatalogRow::from)
            .collect();

        let sort_direction = ajour
            .catalog_header_state
            .previous_sort_direction
            .unwrap_or(SortDirection::Desc);
        let column_key = ajour
            .catalog_header_state
            .previous_column_key
            .unwrap_or(CatalogColumnKey::NumDownloads);

        sort_catalog_addons(&mut catalog_rows, sort_direction, column_key, flavor);

        catalog_rows = catalog_rows
            .into_iter()
            .enumerate()
            .filter_map(|(idx, row)| if idx < result_size { Some(row) } else { None })
            .collect();

        ajour.catalog_search_state.catalog_rows = catalog_rows;
    }
}

pub fn save_column_configs(ajour: &mut Ajour) {
    let my_addons_columns: Vec<_> = ajour
        .header_state
        .columns
        .iter()
        .map(ColumnConfigV2::from)
        .collect();

    let catalog_columns: Vec<_> = ajour
        .catalog_header_state
        .columns
        .iter()
        .map(ColumnConfigV2::from)
        .collect();

    ajour.config.column_config = ColumnConfig::V3 {
        my_addons_columns,
        catalog_columns,
    };

    let _ = ajour.config.save();
}

/// Hardcoded binary names for each compilation target
/// that gets published to the Github Release
const fn bin_name() -> &'static str {
    #[cfg(all(target_os = "windows", feature = "opengl"))]
    {
        "ajour-opengl.exe"
    }

    #[cfg(all(target_os = "windows", feature = "wgpu"))]
    {
        "ajour.exe"
    }

    #[cfg(all(target_os = "macos", feature = "opengl"))]
    {
        "ajour-opengl"
    }

    #[cfg(all(target_os = "macos", feature = "wgpu"))]
    {
        "ajour"
    }

    #[cfg(all(target_os = "linux", feature = "opengl"))]
    {
        "ajour-opengl.AppImage"
    }

    #[cfg(all(target_os = "linux", feature = "wgpu"))]
    {
        "ajour.AppImage"
    }
}
