use std::{fs, path::Path};

use super::engine::TaskRecord;

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn save(tasks_dir: &Path, task: &TaskRecord) -> Result<(), String> {
    if !valid_id(&task.id) {
        return Err("タスク ID が不正です".into());
    }
    fs::create_dir_all(tasks_dir).map_err(|e| format!("タスクの保存先を作れませんでした: {e}"))?;
    let contents = serde_json::to_string_pretty(task)
        .map_err(|e| format!("タスクを JSON に変換できませんでした: {e}"))?;
    let temporary = tasks_dir.join(format!(".{}-{}.tmp", task.id, uuid::Uuid::new_v4()));
    let target = tasks_dir.join(format!("{}.json", task.id));
    let result = (|| {
        fs::write(&temporary, contents)
            .map_err(|e| format!("タスクを書き込めませんでした: {e}"))?;
        fs::rename(&temporary, target).map_err(|e| format!("タスクを保存できませんでした: {e}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub fn load(tasks_dir: &Path) -> Vec<TaskRecord> {
    let Ok(entries) = fs::read_dir(tasks_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            let task: TaskRecord = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            // A copied or edited record must not overwrite another task on recovery.
            (valid_id(&task.id) && path.file_stem().and_then(|s| s.to_str()) == Some(&task.id))
                .then_some(task)
        })
        .collect()
}

pub fn delete(tasks_dir: &Path, id: &str) -> Result<(), String> {
    if !valid_id(id) {
        return Err("タスク ID が不正です".into());
    }
    match fs::remove_file(tasks_dir.join(format!("{id}.json"))) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("タスクを削除できませんでした: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestra::engine::{Source, TaskStatus};

    #[test]
    fn records_round_trip_replace_and_skip_broken_files() {
        let dir = std::env::temp_dir().join(format!("gbd-store-{}", uuid::Uuid::new_v4()));
        let mut task = TaskRecord {
            id: "t0123456789ab".into(),
            project: "C:/project".into(),
            title: "テスト".into(),
            source: Source::Bot,
            created_at: 1,
            updated_at: 1,
            status: TaskStatus::Open,
            pause_reason: None,
            dispatches: 0,
            dispatch_limit: 10,
            runs: Vec::new(),
        };
        save(&dir, &task).unwrap();
        task.status = TaskStatus::Closed;
        save(&dir, &task).unwrap();
        fs::write(dir.join("broken.json"), "{").unwrap();
        fs::write(
            dir.join("wrong-id.json"),
            serde_json::to_string(&task).unwrap(),
        )
        .unwrap();
        let loaded = load(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].status, TaskStatus::Closed);
        assert_eq!(loaded[0].title, "テスト");
        assert!(delete(&dir, "../outside").is_err());
        delete(&dir, &task.id).unwrap();
        assert!(load(&dir).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
}
