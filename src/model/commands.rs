use std::process::{Command, Stdio};
use std::fs::File;
use std::time::{Duration, Instant};
use crate::model::computation_result::ComputationResult;
use wait_timeout::ChildExt;
use serde::{Serialize, Deserialize};
use std::fmt::{Debug, Formatter};
use std::path::PathBuf;
use crate::CHILDREN;
use crate::model::aliases::Aliases;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Commands {
    pub build: String,
    #[serde(default)]
    pub clean: String,
}

#[cfg(target_os = "windows")]
pub fn kill(pid: u32) {
    let _ = Command::new("taskkill")
        .args(&["/PID", &pid.to_string()])
        .spawn()
        .unwrap()
        .wait();
}

#[cfg(target_os = "linux")]
pub fn kill(pid: u32) {
    let _ = Command::new("kill")
        .args(&["-2", &pid.to_string()])
        .spawn()
        .unwrap()
        .wait();
}

impl Commands {
    fn generate_build(&self, shortcuts: &Aliases) -> BuildCommand {
        BuildCommand { sub_command: generate_command(&self.build, shortcuts) }
    }

    fn generate_executable(&self, shortcuts: &Aliases, cmd: &String) -> ExecutableCommand {
        ExecutableCommand { bash_command: restore_str(cmd, shortcuts) }
    }

    fn generate_clean(&self, shortcuts: &Aliases) -> Option<BuildCommand> {
        if self.clean.is_empty() {
            None
        } else {
            Some(BuildCommand { sub_command: generate_command(&self.clean, shortcuts) })
        }
    }

    pub fn run_build(&self, working_directory: &str, shortcuts: &Aliases) {
        let build_command = self.generate_build(shortcuts);
        eprintln!("Building project: ");
        eprintln!("$ {:?}", &build_command.sub_command);
        if !build_command.run(working_directory) {
            panic!("Cannot execute {:?}", build_command.sub_command);
        }
    }

    pub fn run_exec(
        &self,
        working_directory: &str,
        shortcuts: &Aliases,
        cmd: &String,
        err_file: File,
        timeout: Option<Duration>,
    ) -> ComputationResult {
        let executable_command = self.generate_executable(shortcuts, cmd);
        eprintln!("$ {:?}", &executable_command.bash_command);

        if let Some(timeout) = timeout {
            executable_command.run_with_timeout(working_directory, err_file, timeout)
        } else {
            executable_command.run(working_directory, err_file)
        }
    }

    pub fn run_clean(&self, working_directory: &str, shortcuts: &Aliases) {
        if let Some(clean_command) = self.generate_clean(shortcuts) {
            eprintln!("Cleaning project: ");
            eprintln!("$ {:?}", &clean_command.sub_command);
            if !clean_command.run(working_directory) {
                panic!("Cannot execute {:?}", clean_command.sub_command);
            }
        }
    }
}

struct SubCommand {
    executable: String,
    args: Vec<String>,
}

impl Debug for SubCommand {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut content = String::from(&self.executable);
        content.push(' ');
        let mut curr_len = self.executable.len() + 1;

        for (i, element) in self.args.iter().enumerate() {
            content.push_str(element);
            curr_len += element.len();
            if i + 1 < self.args.len() && curr_len + self.args[i + 1].len() > 80 {
                content.push_str(" \\\n   > ");
                curr_len = 0;
            } else if i + 1 < self.args.len() {
                content.push(' ');
                curr_len += 1;
            }
        }

        f.write_str(&content)
    }
}

struct BuildCommand {
    sub_command: SubCommand,
}

impl BuildCommand {
    fn run(&self, working_directory: &str) -> bool {
        Command::new(&self.sub_command.executable)
            .current_dir(working_directory)
            .args(&self.sub_command.args)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

struct ExecutableCommand {
    bash_command: String,
}

impl ExecutableCommand {
    fn run(&self, working_directory: &str, err_file: File) -> ComputationResult {
        let clock = Instant::now();
        let mut child = Command::new("bash")
            .current_dir(working_directory)
            .args(&[ "-c", &self.bash_command ])
            .stderr(Stdio::from(err_file))
            .spawn()
            .expect(&format!("The script cannot execute the following command:\n```\n$ {:?}\n```", self.bash_command));

        let pid = child.id();
        { CHILDREN.lock().unwrap().insert(pid); }
        let success = child.wait()
            .map(|status| status.success());
        { CHILDREN.lock().unwrap().remove(&pid); }

        if let Ok(success) = success {
            if success {
                ComputationResult::Ok(clock.elapsed())
            } else {
                ComputationResult::Error(clock.elapsed())
            }
        } else {
            panic!("\nThe script cannot execute the following command:\n```\n$ {:?}\n```", self.bash_command);
        }
    }

    fn run_with_timeout(&self, working_directory: &str, err_file: File, timeout: Duration) -> ComputationResult {
        let clock = Instant::now();
        let mut child = Command::new("bash")
            .current_dir(working_directory)
            .args(&[ "-c", &self.bash_command ])
            .stderr(Stdio::from(err_file))
            .spawn()
            .expect(&format!("\nThe script cannot execute the following command:\n```\n$ {:?}\n```", self.bash_command));

        let pid = child.id();
        { CHILDREN.lock().unwrap().insert(pid); }

        if let Ok(status) = child.wait_timeout(timeout) {
            { CHILDREN.lock().unwrap().remove(&pid); }
            return if let Some(success) = status.map(|s| s.success()) {
                let _ = child.kill();
                let _ = child.wait();
                if success {
                    ComputationResult::Ok(clock.elapsed())
                } else {
                    ComputationResult::Error(clock.elapsed())
                }
            } else {
                let _ = child.kill();
                let _ = child.wait();
                ComputationResult::Timeout(timeout)
            };
        } else {
            { CHILDREN.lock().unwrap().remove(&pid); }
            panic!();
        }
    }
}

pub fn restore_str(path: &str, shortcuts: &Aliases) -> String {
    let mut path = path.to_owned();
    loop {
        let mut working_copy = path.to_owned();
        for (key, value) in shortcuts.iter() {
            working_copy = working_copy.replace(&format!("{{{}}}", key), &value.to_string());
        }
        if path == working_copy {
            break;
        }
        path = working_copy;
    }
    path
}

pub fn restore_path(path: &PathBuf, shortcuts: &Aliases) -> PathBuf {
    PathBuf::from(restore_str(path.to_str().unwrap(), shortcuts))
}

fn generate_command(command_line: &str, shortcuts: &Aliases) -> SubCommand {
    let full_command = restore_str(command_line, shortcuts);
    let split = full_command.split(' ').collect::<Vec<_>>();
    let (&executable, args) = split.split_first().unwrap();
    let executable = executable.to_owned();
    let args = args.iter().map(|&it| it.to_owned()).collect::<Vec<_>>();
    SubCommand { executable, args }
}