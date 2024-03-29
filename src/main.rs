mod model;
mod tools;

use std::fs::File;
use std::io::{BufReader, BufRead, stdout, Write, stdin, BufWriter, Seek};
use std::path::{Path, PathBuf};

use crate::model::project::{Project, ProjectVersionOnly};
use crate::model::{working_directory, source_directory, log_directory, summary_file, zip_file};
use std::sync::{Arc, Mutex};
use crate::tools::RecursiveZipWriter;
use zip::CompressionMethod;
use ron::ser::PrettyConfig;
use std::ffi::OsStr;
use std::collections::HashSet;
use crate::model::commands::{kill, restore_path};
use termimad::MadSkin;
use std::cmp::Ordering;
use clap::{Parser, Subcommand};
use once_cell::sync::Lazy;
use termimad::crossterm::style::Color;
use threadpool::ThreadPool;
use crate::model::version::Version;

extern crate wait_timeout;
extern crate serde;
extern crate ron;
extern crate humantime;
extern crate clap;

fn parse_duration(v: &str) -> Result<humantime::Duration, String> {
    if let Ok(duration) = v.parse::<humantime::Duration>() {
        Ok(duration)
    } else {
        Err(format!("Cannot parse {} as Duration", v))
    }
}

#[derive(Parser)]
struct CLI {
    path: PathBuf,
    #[clap(subcommand)]
    action: Action,
    #[arg(long)]
    debug: bool,
}

#[derive(Subcommand)]
enum Action {
    Fetch(Fetch),
    Build(Build),
    Run(Run),
    Clean(Clean),
    Zip(Zip),
    Show(Show),
}

#[derive(Parser)]
struct Fetch {
    #[arg(short, long)]
    commit: Option<String>,
}

#[derive(Parser)]
struct Run {
    #[arg(short, long)]
    configuration: Option<PathBuf>,
    #[arg(short, long)]
    overrides: Vec<String>,
    #[arg(long)]
    with_failure: bool,
    #[arg(long)]
    with_in_progress: bool,
    #[arg(long)]
    with_timeout: bool,
    #[arg(short, long)]
    nb_threads: Option<usize>,
    #[arg(short, long, value_parser = parse_duration)]
    global_timeout: Option<humantime::Duration>,
    #[arg(long)]
    only: Option<Vec<String>>,
}

#[derive(Parser)]
struct Build {
    #[arg(short, long)]
    configuration: Option<PathBuf>,
    #[arg(short, long)]
    overrides: Vec<String>,
}

#[derive(Parser)]
struct Clean {
    #[arg(short, long)]
    zip_with: Vec<PathBuf>,
}

#[derive(Parser)]
struct Zip {
    #[arg(short, long)]
    zip_with: Vec<PathBuf>,
}

#[derive(Parser)]
struct Show {
    #[clap(subcommand)]
    action: ShowAction,
}

#[derive(Subcommand)]
enum ShowAction {
    Notes,
    Summary(Summary),
    Status(Status),
    Json(Json),
}

#[derive(Parser)]
struct Summary {
    #[arg(short, long)]
    sort: Option<Vec<String>>,
}

#[derive(Parser)]
struct Status {
    #[arg(short, long)]
    only: Option<Vec<String>>,
}

#[derive(Parser)]
struct Json {
    #[arg(short, long)]
    pretty: bool,
}

fn configure(path: &PathBuf, project: &mut Project) {
    let file = File::open(path)
        .expect(&format!("Cannot open configuration file {:?}", path));

    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line.unwrap();
        let fields = line.split(':').collect::<Vec<_>>();
        let (key, value) = (fields[0], fields[1]);
        project.aliases.insert(key.to_owned(), value.to_owned().parse().unwrap());
    }
}

pub static ABORT: Lazy<Arc<Mutex<bool>>> = Lazy::new(|| Arc::new(Mutex::new(false)));
pub static CHILDREN: Lazy<Arc<Mutex<HashSet<u32>>>> = Lazy::new(|| Arc::new(Mutex::new(HashSet::new())));

const ACCEPTED_VERSIONS: [Version; 4] = [
    Version(0, 5, 0),
    Version(0, 6, 0),
    Version(0, 6, 1),
    Version(0, 6, 2),
];


fn main() {
    let CLI { path, action, debug } = CLI::parse();
    assert!(path.extension() == Some(OsStr::new("zip")) || path.extension() == Some(OsStr::new("ron")));

    let mut config_file = File::open(&path)
        .expect(&format!("Cannot open the configuration file '{:?}'. Maybe the file doesn't exists or the permissions are too restrictive.", path));

    let version = if path.extension() == Some(OsStr::new("zip")) {
        let mut archive = zip::ZipArchive::new(&mut config_file)
            .expect("Cannot read the zip file");
        let mut zip_config_file = archive.by_name("configuration.ron")
            .expect("Cannot read the configuration.ron file. Maybe the archive wasn't build by whitesmith");
        ron::de::from_reader::<_, ProjectVersionOnly>(BufReader::new(&mut zip_config_file))
             .map_err(|e| e.to_string())
             .expect("Cannot parse the configuration file")
    } else {
        ron::de::from_reader::<_, ProjectVersionOnly>(BufReader::new(&mut config_file))
             .map_err(|e| e.to_string())
             .expect("Cannot parse the configuration file")
    };
    config_file.rewind().unwrap();

    if !ACCEPTED_VERSIONS.contains(&version.version) {
        panic!("{:?} is not accepted by the current whitesmith instance. Valid versions are: {:?}", &version.version, &ACCEPTED_VERSIONS.map(|it| it.to_string()));
    }

    let (mut project, is_zip_archive) = if path.extension() == Some(OsStr::new("zip")) {
        let mut archive = zip::ZipArchive::new(config_file)
            .expect("Cannot read the zip file");
        let zip_config_file = archive.by_name("configuration.ron")
            .expect("Cannot read the configuration.ron file. Maybe the archive wasn't build by whitesmith");
        (ron::de::from_reader::<_, Project>(BufReader::new(zip_config_file))
             .map_err(|e| e.to_string())
             .expect("Cannot parse the configuration file"), true)
    } else {
        (ron::de::from_reader::<_, Project>(BufReader::new(config_file))
             .map_err(|e| e.to_string())
             .expect("Cannot parse the configuration file"), false)
    };

    project.working_directory = working_directory(&path, &project.versioning);
    project.source_directory = source_directory(&path, &project.versioning);
    project.log_directory = log_directory(&path, &project.versioning);
    project.summary_file = summary_file(&path, &project.versioning, is_zip_archive);
    project.debug = debug;

    project.aliases.insert(String::from("PROJECT"), project.working_directory.to_owned().parse().unwrap());
    project.aliases.insert(String::from("SOURCES"), project.source_directory.to_owned().parse().unwrap());
    project.aliases.insert(String::from("LOGS"), project.log_directory.to_owned().parse().unwrap());
    project.aliases.insert(String::from("SUMMARY_FILE"), project.summary_file.to_owned().parse().unwrap());

    project.init();

    let zip_path = zip_file(&path, &project);

    match action {
        Action::Fetch(fetch_args) => {
            if let Some(commit) = fetch_args.commit {
                project.versioning.commit = Some(commit);
            }
            project.fetch_sources();
        }
        Action::Build(build_args) => {
            if let Some(path) = build_args.configuration {
                configure(&path, &mut project);
            }
            for _override in build_args.overrides {
                let fields = _override.split(':').collect::<Vec<_>>();
                let (key, value) = (fields[0], fields[1]);
                project.aliases.insert(key.to_owned(), value.to_owned().parse().unwrap());
            }
            project.build();
        }
        Action::Run(run_args) => {
            if let Some(path) = run_args.configuration {
                configure(&path, &mut project);
            }

            for _override in run_args.overrides {
                let fields = _override.split(':').collect::<Vec<_>>();
                let (key, value) = (fields[0], fields[1]);
                project.aliases.insert(key.to_owned(), value.to_owned().parse().unwrap());
            }
            if let Some(duration) = run_args.global_timeout {
                project.global_timeout = Some(duration.into());
            }
            if let Ok(file) = File::create(Path::new(&project.working_directory).join("last_running_configuration.ron")) {
                let writer = BufWriter::new(file);
                ron::ser::to_writer_pretty(writer, &project, PrettyConfig::default())
                    .expect("Cannot serialize the project file to toml");
            }
            let project = Arc::new(project);
            run_project(
                project.clone(),
                run_args.nb_threads,
                run_args.with_in_progress,
                run_args.with_timeout,
                run_args.with_failure,
            );
        }
        Action::Clean(clean_args) => {
            if Path::new(&project.summary_file).exists() {
                let valid_answers = ["", "y", "Y", "n", "N"];
                let mut answer = String::new();
                loop {
                    eprint!("The project has been executed. Would you save the previous results before cleaning the project ? [Y/n] ");
                    stdout().flush().unwrap();
                    stdin().read_line(&mut answer).expect("Cannot read stdin");
                    let answer = answer.trim();
                    if valid_answers.iter().any(|&it| it == answer) {
                        break;
                    }
                }

                let positive_answers = &valid_answers[0..3];
                let answer = answer.trim();
                if positive_answers.contains(&answer) {
                    let zip_path = zip_path.replace(".zip", ".backup.zip");
                    zip_project(&zip_path, &project, &clean_args.zip_with);
                }
            }
            project.clean();
        }
        Action::Show(show_args) => {
            match show_args.action {
                ShowAction::Notes => print_notes(&project),
                ShowAction::Summary(Summary { sort }) => {
                    eprintln!("{}", &project.summary_file);
                    let sort_columns = sort;
                    let result = if is_zip_archive {
                        /*let mut archive = zip::ZipArchive::new(String::new()).unwrap();
                        let summary_file = archive.by_name(&project.summary_file).unwrap();
                        let mut reader = BufReader::new(summary_file);
                        print_summary(&mut reader, sort_columns)*/
                        Ok(())
                    } else {
                        if let Ok(summary_file) = File::open(&project.summary_file) {
                            let mut reader = BufReader::new(summary_file);
                            print_summary(&mut reader, sort_columns)
                        } else {
                            Ok(())
                        }
                    };
                    result.expect("Cannot read the summary file");
                }
                ShowAction::Status(Status { only }) => {
                    project.display_status(&only);
                }
                ShowAction::Json(Json { pretty }) => {
                    if pretty {
                        println!("{}", serde_json::ser::to_string_pretty(&project).unwrap());
                    } else {
                        println!("{}", serde_json::ser::to_string(&project).unwrap());
                    }
                }
            }
        }
        Action::Zip(zip) => {
            zip_project(&zip_path, &project, &zip.zip_with);
        }
    }
}

fn print_summary<RS>(reader: &mut BufReader<RS>, sort_columns: Option<Vec<String>>) -> std::io::Result<()>
    where RS: std::io::Read {
    let mut col_sizes = Vec::new();
    let mut lines = Vec::new();

    let mut headers = None;

    for line in reader.lines() {
        let line = line?;
        let parts = line.split('\t')
            .map(String::from)
            .collect::<Vec<_>>();
        if let None = headers {
            headers = Some(parts.clone());
        }
        let parts_len = parts.iter()
            .map(&String::len)
            .collect::<Vec<_>>();
        let mut i = 0;
        while i < usize::min(col_sizes.len(), parts.len()) {
            col_sizes[i] = usize::max(col_sizes[i], parts_len[i]);
            i += 1;
        }

        while col_sizes.len() < parts.len() {
            col_sizes.push(parts_len[i]);
            i += 1;
        }
        lines.push(parts);
    }

    if let Some(header) = headers {
        if let Some(sort_columns) = sort_columns {
            let empty_string = String::new();
            lines[1..].sort_by(|lhs, rhs| {
                for column in &sort_columns {
                    let (column, rev) = if column.starts_with('~') {
                        (column.chars().skip(1).collect::<String>(), true)
                    } else {
                        (column.to_string(), false)
                    };

                    if let Some(index) = header.iter().position(|it| it.eq_ignore_ascii_case(&column)) {
                        let mut comparison = human_sort::compare(
                            &lhs.get(index).unwrap_or(&empty_string),
                            &rhs.get(index).unwrap_or(&empty_string),
                        );

                        if rev { comparison = comparison.reverse(); }

                        if comparison != Ordering::Equal {
                            return comparison;
                        }
                    }
                }
                Ordering::Equal
            });
        }
    }

    for line in lines {
        for (i, part) in line.iter().enumerate() {
            eprint!("{:1$}", part, col_sizes[i] + 3);
        }
        eprintln!();
    }

    Ok(())
}

fn zip_project(zip_path: &str, project: &Project, files_to_add: &Vec<PathBuf>) {
    let zip_file = File::create(zip_path)
        .expect("Cannot create the zip archive");
    let mut archive = RecursiveZipWriter::new(zip_file)
        .compression_method(CompressionMethod::Stored);

    let mut paths = HashSet::new();

    archive.add_path(Path::new(&project.log_directory))
        .expect("Fail to add the log directory to the zip archive");
    paths.insert(PathBuf::from(&project.log_directory));

    archive.add_path(Path::new(&project.summary_file))
        .expect("Fail to add the summary file to the zip archive");
    paths.insert(PathBuf::from(&project.summary_file));

    archive.add_path(Path::new(&project.working_directory).join("last_running_configuration.ron").as_path())
        .expect("Cannot add the running configuration file to the zip archive");
    paths.insert(PathBuf::from(&project.working_directory).join("last_running_configuration.ron"));

    let serialized_project = ron::ser::to_string_pretty(project, PrettyConfig::default())
        .expect("Cannot serialize the project file to toml");
    archive.add_buf(serialized_project.as_bytes(), Path::new("configuration.ron"))
        .expect("Fail to add the configuration file to the zip archive");
    paths.insert(PathBuf::from("configuration.ron"));

    for file_to_add in &project.zip_with {
        let full_path = restore_path(&PathBuf::from(&file_to_add), &project.aliases);
        if !paths.contains(&full_path) {
            archive.add_path(&full_path)
                .expect(&format!("Fail to add {} to the zip archive", file_to_add));
            paths.insert(full_path);
        }
    }
    for file_to_add in files_to_add.iter() {
        let full_path = restore_path(file_to_add, &project.aliases);
        if !paths.contains(&full_path) {
            archive.add_path(&full_path)
                .expect(&format!("Fail to add {:?} to the zip archive", file_to_add));
            paths.insert(full_path);
        }
    }


    let archive = archive.finish()
        .expect("Fail to build the archive");

    eprintln!("{:?}", archive);
}

fn print_notes(project: &Project) {
    if let Some(description) = &project.description {
        let mut description = description.trim().to_owned();

        description.insert_str(0, "\n---\n");
        description.push_str("\n---\n");

        let mut skin = MadSkin::default_dark();
        skin.bold.set_fg(Color::Red);
        skin.print_text(&description);

        // eprintln!("{}", &description);
    } else {
        eprintln!("The configuration doesn't contain notes.")
    }
}

fn run_project(
    project: Arc<Project>,
    nb_threads: Option<usize>,
    with_in_progress: bool,
    with_timeout: bool,
    with_failure: bool,
) {
    if project.requires_overrides() {
        return;
    }

    if with_in_progress {
        project.unlock_in_progress();
    }

    if with_timeout {
        project.unlock_timeout();
    }

    if with_failure {
        project.unlock_failed();
    }

    ctrlc::set_handler(|| {
        { *ABORT.lock().unwrap() = true; }
        let children = CHILDREN.lock().unwrap();
        for &child in children.iter() {
            eprintln!("Send Kill to {}", child);
            kill(child);
        }
        std::process::exit(2);
    }).expect("Cannot init CTRL-C handler");

    if let Some(limits) = &project.limits {
        if let Err(e) = limits.apply() {
            eprintln!("Cannot apply project limitations\n{:?}", e);
        }
    }

    let pool = ThreadPool::new(nb_threads.unwrap_or(1));
    project.run(pool.clone());
    pool.join();
}