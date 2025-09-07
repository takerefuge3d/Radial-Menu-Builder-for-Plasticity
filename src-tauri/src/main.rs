#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tauri::Manager;
// use tauri_plugin_dialog;
// use tauri_plugin_fs;
// use tauri_plugin_shell;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct MenuItem {
    command: String,
    icon: String,
    label: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RadialMenu {
    name: String,
    command: String,
    items: Vec<MenuItem>,
}

#[tauri::command]
async fn load_commands() -> Result<HashMap<String, String>, String> {
    // Try multiple possible locations for commands.json
    let possible_paths = [
        "dist/commands.json",
        "./dist/commands.json", 
        "../dist/commands.json",
        "commands.json"
    ];
    
    for path in &possible_paths {
        if let Ok(content) = fs::read_to_string(path) {
            match serde_json::from_str::<HashMap<String, String>>(&content) {
                Ok(commands) => {
                    println!("Successfully loaded {} commands from: {}", commands.len(), path);
                    return Ok(commands);
                }
                Err(e) => println!("Failed to parse commands from {}: {}", path, e),
            }
        } else {
            println!("Commands file not found at: {}", path);
        }
    }
    
    println!("Could not find commands.json, using fallback commands");
    
    // Fallback to hardcoded commands
    let mut commands = HashMap::new();
    commands.insert("command:align".to_string(), "align".to_string());
    commands.insert("command:boolean".to_string(), "boolean".to_string());
    // Add more fallback commands as needed
    
    Ok(commands)
}

#[tauri::command]
async fn load_commands_from_file(path: String) -> Result<HashMap<String, String>, String> {
    match fs::read_to_string(&path) {
        Ok(content) => {
            match serde_json::from_str::<HashMap<String, String>>(&content) {
                Ok(commands) => {
                    println!("Loaded {} commands from custom file: {}", commands.len(), path);
                    Ok(commands)
                }
                Err(e) => Err(format!("Failed to parse commands file: {}", e)),
            }
        }
        Err(e) => Err(format!("Failed to read file: {}", e)),
    }
}

#[tauri::command]
async fn save_radial_menu(menu: RadialMenu, path: String) -> Result<(), String> {
    let json = serde_json::to_string_pretty(&menu)
        .map_err(|e| format!("Failed to serialize menu: {}", e))?;
    
    fs::write(&path, json)
        .map_err(|e| format!("Failed to write file: {}", e))?;
    
    println!("Saved radial menu '{}' to: {}", menu.name, path);
    Ok(())
}

#[tauri::command]
async fn load_radial_menu(path: String) -> Result<RadialMenu, String> {
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read file: {}", e))?;
    
    let menu: RadialMenu = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse menu: {}", e))?;
    
    println!("Loaded radial menu '{}' from: {}", menu.name, path);
    Ok(menu)
}

#[tauri::command]
async fn list_json_files(directory: String) -> Result<Vec<String>, String> {
    let path = Path::new(&directory);
    
    if !path.exists() {
        return Err("Directory does not exist".to_string());
    }
    
    let mut files = Vec::new();
    
    match fs::read_dir(path) {
        Ok(entries) => {
            for entry in entries {
                if let Ok(entry) = entry {
                    let file_path = entry.path();
                    if let Some(extension) = file_path.extension() {
                        if extension == "json" {
                            if let Some(file_name) = file_path.file_name() {
                                if let Some(name) = file_name.to_str() {
                                    files.push(name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(e) => return Err(format!("Failed to read directory: {}", e)),
    }
    
    files.sort();
    println!("Found {} JSON files in directory: {}", files.len(), directory);
    Ok(files)
}

#[tauri::command]
async fn save_radials_directory(app_handle: tauri::AppHandle, path: String) -> Result<(), String> {
    let app_dir = app_handle.path()
        .app_config_dir()
        .map_err(|e| format!("Failed to get app config directory: {}", e))?;
    
    fs::create_dir_all(&app_dir)
        .map_err(|e| format!("Failed to create config directory: {}", e))?;
    
    let config_file = app_dir.join("radials_directory.txt");
    fs::write(config_file, path.clone())
        .map_err(|e| format!("Failed to save radials directory: {}", e))?;
    
    println!("Saved radials directory preference: {}", path);
    Ok(())
}

#[tauri::command]
async fn get_saved_radials_directory(app_handle: tauri::AppHandle) -> Result<Option<String>, String> {
    let app_dir = app_handle.path()
        .app_config_dir()
        .map_err(|e| format!("Failed to get app config directory: {}", e))?;
    
    let config_file = app_dir.join("radials_directory.txt");
    
    if config_file.exists() {
        let content = fs::read_to_string(config_file)
            .map_err(|e| format!("Failed to read radials directory: {}", e))?;
        
        let path = content.trim().to_string();
        
        if Path::new(&path).exists() {
            println!("Loaded saved radials directory: {}", path);
            Ok(Some(path))
        } else {
            println!("Saved radials directory no longer exists: {}", path);
            Ok(None)
        }
    } else {
        println!("No saved radials directory found");
        Ok(None)
    }
}

fn main() {
    tauri::Builder::default()
        // .plugin(tauri_plugin_dialog::init())
        // .plugin(tauri_plugin_fs::init())
        // .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            load_commands,
            load_commands_from_file,
            save_radial_menu,
            load_radial_menu,
            list_json_files,
            save_radials_directory,
            get_saved_radials_directory
        ])
        .setup(|app| {
            // Create examples directory if it doesn't exist
            let app_handle = app.handle();
            if let Ok(app_dir) = app_handle.path().app_data_dir() {
                let examples_dir = app_dir.join("examples");
                
                if !examples_dir.exists() {
                    if let Err(e) = std::fs::create_dir_all(&examples_dir) {
                        println!("Failed to create examples directory: {}", e);
                    } else {
                        // Create example radial menu
                        let example_menu = RadialMenu {
                            name: "ADD menu".to_string(),
                            command: "your-name:add-menu".to_string(),
                            items: vec![
                                MenuItem {
                                    command: "command:line".to_string(),
                                    icon: "line".to_string(),
                                    label: "Line".to_string(),
                                },
                                MenuItem {
                                    command: "command:curve".to_string(),
                                    icon: "curve".to_string(),
                                    label: "Curve".to_string(),
                                },
                                MenuItem {
                                    command: "command:center-circle".to_string(),
                                    icon: "center-circle".to_string(),
                                    label: "Center Circle".to_string(),
                                },
                                MenuItem {
                                    command: "command:polygon".to_string(),
                                    icon: "polygon".to_string(),
                                    label: "Polygon".to_string(),
                                },
                                MenuItem {
                                    command: "command:sphere".to_string(),
                                    icon: "sphere".to_string(),
                                    label: "Sphere".to_string(),
                                },
                            ],
                        };
                        
                        let example_path = examples_dir.join("example.radial.json");
                        if let Ok(json) = serde_json::to_string_pretty(&example_menu) {
                            if let Err(e) = std::fs::write(example_path, json) {
                                println!("Failed to write example menu: {}", e);
                            }
                        }
                    }
                }
            }
            
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}