#![deny(warnings)]
#![feature(inclusive_range_syntax)]

extern crate orbclient;
extern crate orbimage;
extern crate orbfont;
extern crate orbtk;
extern crate mime_guess;
extern crate mime;

use std::{cmp, env, fs};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::string::{String, ToString};
use std::vec::Vec;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::sync::Arc;

use mime::TopLevel as MimeTop;

use orbclient::{Color, Renderer};
use orbimage::Image;

use orbtk::{Window, Point, Rect, List, Entry, Label, Place, Text, Click};

const ICON_SIZE: i32 = 32;

#[cfg(target_os = "redox")]
static UI_PATH: &'static str = "/ui/icons";

#[cfg(not(target_os = "redox"))]
static UI_PATH: &'static str = "ui/icons";

#[cfg(target_os = "redox")]
static LAUNCH_COMMAND: &'static str = "/ui/bin/launcher";

#[cfg(not(target_os = "redox"))]
static LAUNCH_COMMAND: &'static str = "xdg-open";

struct FileInfo {
    name: String,
    full_path: String,
    size: u64,
    size_str: String,
    is_dir: bool,
}

impl FileInfo {
    fn new(name: String, full_path: String, is_dir: bool) -> FileInfo {
        let (size, size_str) = {
            if is_dir {
                FileManager::get_num_entries(&full_path)
            } else {
                match fs::metadata(&full_path) {
                    Ok(metadata) => {
                        let size = metadata.len();
                        if size >= 1_000_000_000 {
                            (size, format!("{:.1} GB", (size as u64) / 1_000_000_000))
                        } else if size >= 1_000_000 {
                            (size, format!("{:.1} MB", (size as u64) / 1_000_000))
                        } else if size >= 1_000 {
                            (size, format!("{:.1} KB", (size as u64) / 1_000))
                        } else {
                            (size, format!("{:.1} bytes", size))
                        }
                    }
                    Err(err) => (0, format!("Failed to open: {}", err)),
                }
            }
        };
        FileInfo {
            name: name,
            full_path: full_path,
            size: size,
            size_str: size_str,
            is_dir: is_dir,
        }
    }
}

struct FileType {
    description: String,
    icon: PathBuf
}

impl FileType {
    fn new(desc: String, icon: &'static str) -> FileType {
        for folder in ["mimetypes", "places"].iter() {
            let mut path = fs::canonicalize(UI_PATH).unwrap();
            path.push(folder);
            path.push(format!("{}.png", icon));
            if path.is_file() {
                return FileType {
                    description: desc,
                    icon: path,
                };
            } else {
                println!("{} not found in {}", icon, folder);
            }
        }

        println!("{} not found", icon);
        let mut path = fs::canonicalize(UI_PATH).unwrap();
        path.push("mimetypes/unknown.png");
        FileType {
            description: desc,
            icon: path,
        }
    }

    fn from_filename(file_name: &str) -> Self {
        if file_name.ends_with('/') {
            Self::new("folder".to_owned(), "inode-directory")
        } else {
            let pos = file_name.rfind('.').unwrap_or(0) + 1;
            let ext = &file_name[pos..];
            let mime = mime_guess::get_mime_type(ext);
            let image = match (&mime.0, &mime.1) {
                (&MimeTop::Image, _) => "image-x-generic",
                (&MimeTop::Text, _) => "text-plain",
                (&MimeTop::Audio, _) => "audio-x-generic",
                _ => match ext {
                    "c" | "cpp" | "h" => "text-x-c",
                    "asm" | "ion" | "lua" | "rc" | "rs" | "sh" => "text-x-script",
                    "ttf" => "application-x-font-ttf",
                    "tar" => "package-x-generic",
                    _ => "unknown"
                }
            };
            Self::new(format!("{}", mime), image)
        }
    }
}

struct FileTypesInfo {
    images: BTreeMap<PathBuf, Image>,
}

impl FileTypesInfo {
    pub fn new() -> FileTypesInfo {
        FileTypesInfo { images: BTreeMap::new() }
    }

    pub fn description_for(&self, file_name: &str) -> String {
        FileType::from_filename(file_name).description
    }

    pub fn icon_for(&mut self, file_name: &str) -> &Image {
        let icon = FileType::from_filename(file_name).icon;

        if ! self.images.contains_key(&icon) {
            self.images.insert(icon.clone(), load_icon(&icon));
        }
        &self.images[&icon]
    }
}

enum FileManagerCommand {
    ChangeDir(String),
    Execute(String),
    ChangeSort(usize),
}

#[derive(PartialEq)]
enum SortPredicate {
    Name,
    Size,
    Type,
}

#[derive(PartialEq)]
enum SortDirection {
    Asc,
    Desc,
}

impl SortDirection {
    fn invert(&mut self) {
        match *self {
            SortDirection::Asc => *self = SortDirection::Desc,
            SortDirection::Desc => *self = SortDirection::Asc,
        }
    }
}

struct Column {
    name: &'static str,
    x: i32,
    width: i32,
    sort_predicate: SortPredicate,
}

pub struct FileManager {
    file_types_info: FileTypesInfo,
    files: Vec<FileInfo>,
    columns: [Column; 3],
    column_labels: Vec<Arc<Label>>,
    sort_predicate: SortPredicate,
    sort_direction: SortDirection,
    window: Window,
    list_widget_index: Option<usize>,
    tx: Sender<FileManagerCommand>,
    rx: Receiver<FileManagerCommand>,
}

fn load_icon(path: &Path) -> Image {
    match Image::from_path(path) {
        Ok(icon) => if icon.width() == ICON_SIZE as u32 && icon.height() == ICON_SIZE as u32 {
            icon
        } else {
            icon.resize(ICON_SIZE as u32, ICON_SIZE as u32, orbimage::ResizeType::Lanczos3).unwrap()
        },
        Err(err) => {
            println!("Failed to load icon {}: {}", path.display(), err);
            Image::from_color(ICON_SIZE as u32, ICON_SIZE as u32, Color::rgba(0, 0, 0, 0))
        }
    }
}

impl FileManager {
    pub fn new() -> Self {
        let (tx, rx) = channel();

        FileManager {
            file_types_info: FileTypesInfo::new(),
            files: Vec::new(),
            columns: [
                Column {
                    name: "Name",
                    x: 0,
                    width: 0,
                    sort_predicate: SortPredicate::Name,
                },
                Column {
                    name: "Size",
                    x: 0,
                    width: 0,
                    sort_predicate: SortPredicate::Size,
                },
                Column {
                    name: "Type",
                    x: 0,
                    width: 0,
                    sort_predicate: SortPredicate::Type,
                },
            ],
            column_labels: Vec::new(),
            sort_predicate: SortPredicate::Name,
            sort_direction: SortDirection::Asc,
            window: Window::new(Rect::new(-1, -1, 0, 0),  ""),
            list_widget_index: None,
            tx: tx,
            rx: rx,
        }
    }

    fn get_parent_directory(path: &str) -> Option<String> {
        match fs::canonicalize(path.to_owned() + "../") {
            Ok(parent) => {
                let mut parent = parent.into_os_string().into_string().unwrap_or("/".to_string());
                if ! parent.ends_with('/') {
                    parent.push('/');
                }

                if parent == path {
                    return None
                } else {
                    return Some(parent);
                }
            },
            Err(err) => println!("failed to get path: {}", err)
        }

        None
    }

    fn get_num_entries(path: &str) -> (u64, String) {
        let count = match fs::read_dir(path) {
            Ok(entry_readdir) => entry_readdir.count(),
            Err(_) => 0,
        };
        if count == 1 {
            (count as u64, "1 entry".to_string())
        } else {
            (count as u64, format!("{} entries", count))
        }
    }

    fn push_file(&mut self, file_info: FileInfo) {
        let description = self.file_types_info.description_for(&file_info.name);
        self.columns[0].width = cmp::max(self.columns[0].width, (file_info.name.len() * 8) as i32 + 16);
        self.columns[1].width = cmp::max(self.columns[1].width, (file_info.size_str.len() * 8) as i32 + 16);
        self.columns[2].width = cmp::max(self.columns[2].width, (description.len() * 8) as i32 + 16);

        self.files.push(file_info);
    }

    fn update_headers(&mut self) {
        for (i, column) in self.columns.iter().enumerate() {
            if let None = self.column_labels.get(i * 2) {
                // header text
                let mut label = Label::new();
                self.window.add(&label);
                label.bg.set(Color::rgba(255, 255, 255, 0));
                label.text_offset.set(Point::new(0, 8));

                let tx = self.tx.clone();
                label.on_click(move |_, _| {
                    tx.send(FileManagerCommand::ChangeSort(i)).unwrap();
                });
                self.column_labels.push(label);

                // sort arrow
                label = Label::new();
                self.window.add(&label);
                label.bg.set(Color::rgba(255, 255, 255, 0));
                label.fg.set(Color::rgb(140, 140, 140));
                label.text_offset.set(Point::new(0, 8));
                self.column_labels.push(label);
            }

            if let Some(label) = self.column_labels.get(i * 2) {
                label.position(column.x, 0).size(column.width as u32, 32).text(column.name.clone());
            }

            if let Some(label) = self.column_labels.get(i * 2 + 1) {
                if column.sort_predicate == self.sort_predicate {
                    let arrow = match self.sort_direction {
                        SortDirection::Asc => "↓",
                        SortDirection::Desc => "↑",
                    };

                    label.position(column.x + column.width - 12, 0).size(16, 32).text(arrow);
                } else {
                    label.text("");
                }
            }
        }
    }

    fn update_list(&mut self) {
        let w = (self.columns[2].x + self.columns[2].width) as u32;
        let count = cmp::min(self.files.len(), 7);
        let h = if self.files.len() < 8 {
            (count * ICON_SIZE as usize) as u32 + 32 // +32 for the header row
        } else {
            (7 * ICON_SIZE as usize) as u32 + 32 - 16 // +32 for the header row, -16 to indicate scrolling
        };

        let list = List::new();
        list.position(0, 32).size(w, h - 32);

        {
            for file in self.files.iter() {
                let entry = Entry::new(ICON_SIZE as u32);

                let path = file.full_path.clone();
                let tx = self.tx.clone();

                entry.on_click(move |_, _| {
                    if path.ends_with('/') {
                        tx.send(FileManagerCommand::ChangeDir(path.clone())).unwrap();
                    } else {
                        tx.send(FileManagerCommand::Execute(path.clone())).unwrap();
                    }
                });

                {
                    let icon = self.file_types_info.icon_for(&file.name);
                    let image = orbtk::Image::from_image((*icon).clone());
                    image.position(4, 0);
                    entry.add(&image);
                }

                let mut label = Label::new();
                label.position(self.columns[0].x, 0).size(w, ICON_SIZE as u32).text(file.name.clone());
                label.text_offset.set(Point::new(0, 8));
                label.bg.set(Color::rgba(255, 255, 255, 0));
                entry.add(&label);

                label = Label::new();
                label.position(self.columns[1].x, 0).size(w, ICON_SIZE as u32).text(file.size_str.clone());
                label.text_offset.set(Point::new(0, 8));
                label.bg.set(Color::rgba(255, 255, 255, 0));
                entry.add(&label);

                let description = self.file_types_info.description_for(&file.name);
                label = Label::new();
                label.position(self.columns[2].x, 0).size(w, ICON_SIZE as u32).text(description);
                label.text_offset.set(Point::new(0, 8));
                label.bg.set(Color::rgba(255, 255, 255, 0));
                entry.add(&label);

                list.push(&entry);
            }
        }

        if let Some(i) = self.list_widget_index {
            let mut widgets = self.window.widgets.borrow_mut();
            widgets.remove(i);
            widgets.insert(i, list);
        } else {
            self.list_widget_index = Some(self.window.add(&list));
        }
    }

    fn set_path(&mut self, path: &str) {
        for column in self.columns.iter_mut() {
            column.width = (column.name.len() * 8) as i32 + 16;
        }

        self.files.clear();

        // check to see if parent directory exists
        if let Some(parent) = FileManager::get_parent_directory(path) {
            self.push_file(FileInfo::new("../".to_string(), parent, true));
        }

        match fs::read_dir(path) {
            Ok(readdir) => {
                for entry_result in readdir {
                    match entry_result {
                        Ok(entry) => {
                            let directory = match entry.file_type() {
                                Ok(file_type) => file_type.is_dir(),
                                Err(err) => {
                                    println!("Failed to read file type: {}", err);
                                    false
                                }
                            };

                            let entry_path = match entry.file_name().to_str() {
                                Some(path_str) => if directory {
                                    path_str.to_string() + "/"
                                } else {
                                    path_str.to_string()
                                },
                                None => {
                                    println!("Failed to read file name");
                                    String::new()
                                }
                            };

                            let full_path = path.to_owned() + entry_path.clone().as_str();
                            self.push_file(FileInfo::new(entry_path, full_path, directory));
                        },
                        Err(err) => println!("failed to read dir entry: {}", err)
                    }
                }

            },
            Err(err) => {
                println!("failed to readdir {}: {}", path, err);
            },
        }

        self.columns[0].x = ICON_SIZE + 8;
        self.columns[1].x = self.columns[0].x + self.columns[0].width;
        self.columns[2].x = self.columns[1].x + self.columns[1].width;

        let w = (self.columns[2].x + self.columns[2].width) as u32;
        let count = cmp::min(self.files.len(), 7);
        let h = if self.files.len() < 8 {
            (count * ICON_SIZE as usize) as u32 + 32 // +32 for the header row
        } else {
            (7 * ICON_SIZE as usize) as u32 + 32 - 16 // +32 for the header row, -16 to indicate scrolling
        };

        self.window.set_size(w, h);
        self.window.set_title(&path);
        self.window.bg.set(Color::rgb(255, 255, 255));

        self.sort_files();

        self.update_headers();

        self.update_list();

        self.window.needs_redraw();
    }

    fn sort_files(&mut self) {
        match self.sort_predicate {
            SortPredicate::Name => self.files.sort_by(|a, b| a.name.cmp(&b.name)),
            SortPredicate::Size => {
                self.files.sort_by(|a, b|
                    if a.is_dir != b.is_dir {
                        b.is_dir.cmp(&a.is_dir) // Sort directories first
                    } else {
                        a.size.cmp(&b.size)
                    })
            },
            SortPredicate::Type => {
                let file_types_info = &self.file_types_info;
                self.files.sort_by_key(|file| file_types_info.description_for(&file.name).to_lowercase())
            },
        }
        if self.sort_direction == SortDirection::Desc {
            self.files.reverse();
        }
    }

    fn main(&mut self, path: &str) {
        // Filter out invalid paths
        let mut path = match fs::canonicalize(path.to_owned()) {
            Ok(p) => p.into_os_string().into_string().unwrap_or("file:/".to_owned()),
            _ => "file:/".to_owned(),
        };
        if ! path.ends_with('/') {
            path.push('/');
        }

        println!("main path: {}", path);
        self.set_path(&path);
        self.window.draw_if_needed();

        while self.window.running.get() {

            self.window.step();

            while let Ok(event) = self.rx.try_recv() {

                match event {
                    FileManagerCommand::ChangeDir(dir) => {
                        self.set_path(&dir);
                    }
                    FileManagerCommand::Execute(cmd) => {
                        Command::new(LAUNCH_COMMAND).arg(&cmd).spawn().unwrap();
                    },
                    FileManagerCommand::ChangeSort(i) => {
                        let predicate = match i {
                            0 => SortPredicate::Name,
                            1 => SortPredicate::Size,
                            2 => SortPredicate::Type,
                            _ => return
                        };

                        if self.sort_predicate != predicate {
                            self.sort_predicate = predicate;
                        } else {
                            self.sort_direction.invert();
                        }

                        self.update_headers();
                        self.sort_files();
                        self.update_list();
                    },
                }
            }

            self.window.draw_if_needed();
        }
    }
}

fn main() {
    match env::args().nth(1) {
        Some(ref arg) => FileManager::new().main(arg),
        None => if let Some(home) = env::home_dir() {
            FileManager::new().main(home.into_os_string().to_str().unwrap_or("."))
        } else {
            FileManager::new().main(".")
        }
    }
}
