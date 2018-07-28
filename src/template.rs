use std::env;
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::io::Read;
use std::fs::{self, File};
use std::str;

use toml::{self, Value};
use tera::{Tera, Context};
use walkdir::WalkDir;
use git2::Repository;
use glob::Pattern;

use errors::{Result, ErrorKind, new_error};
use prompt::{ask_string, ask_bool, ask_choices, ask_integer};
use utils::{Source, get_source, read_file, write_file, create_directory};
use utils::{is_vcs, is_binary};
use definition::TemplateDefinition;


#[derive(Debug, PartialEq)]
pub struct Template {
    /// Local path to the template folder
    path: PathBuf,
}

impl Template {
    pub fn from_input(input: &str) -> Result<Template> {
        match get_source(input) {
            Source::Git(remote) => Template::from_git(&remote),
            Source::Local(path) => Ok(Template::from_local(&path)),
        }
    }

    pub fn from_git(remote: &str) -> Result<Template> {
        // Clone the remote in git first in /tmp
        let mut tmp = env::temp_dir();
        tmp.push(remote.split("/").last().unwrap_or_else(|| "kickstart"));
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        println!("Cloning the repository in your temporary folder...");

        match Repository::clone(remote, &tmp) {
            Ok(_) => (),
            Err(e) => return Err(new_error(ErrorKind::Git(e))),
        };

        Ok(Template::from_local(&tmp))
    }

    pub fn from_local(path: &PathBuf) -> Template {
        Template {
            path: path.to_path_buf(),
        }
    }

    fn ask_questions(&self, def: &TemplateDefinition) -> Result<Context> {
        let mut context = Context::new();
        // Tera context doesn't expose a way to get value from a context
        // so we store them in another hashmap
        let mut vals = HashMap::new();

        for var in &def.variables {
            // Skip the question if the value is different from the condition
            if let Some(ref cond) = var.only_if {
                if let Some(val) = vals.get(&cond.name) {
                    if *val != cond.value {
                        continue;
                    }
                }
            }

            if let Some(ref choices) = var.choices {
                let res = ask_choices(&var.prompt, &var.default, choices)?;
                context.add(&var.name, &res);
                vals.insert(var.name.clone(), res);
                continue;
            }

            match &var.default {
                Value::Boolean(b) => {
                    let res = ask_bool(&var.prompt, *b)?;
                    context.add(&var.name, &res);
                    vals.insert(var.name.clone(), Value::Boolean(res));
                    continue;
                },
                Value::String(s) => {
                    let res = ask_string(&var.prompt, &s)?;
                    context.add(&var.name, &res);
                    vals.insert(var.name.clone(), Value::String(res));
                    continue;
                },
                Value::Integer(i) => {
                    let res = ask_integer(&var.prompt, *i)?;
                    context.add(&var.name, &res);
                    vals.insert(var.name.clone(), Value::Integer(res));
                    continue;
                },
                _ => panic!("Unsupported TOML type in a question: {:?}", var.default)
            }
        }

        Ok(context)
    }

    pub fn generate(&self, output_dir: &PathBuf) -> Result<()> {
        // Get the variables from the user first
        let conf_path = self.path.join("template.toml");
        if !conf_path.exists() {
            return Err(new_error(ErrorKind::MissingTemplateDefinition));
        }

        let definition: TemplateDefinition = toml::from_str(&read_file(&conf_path)?)
            .map_err(|_| new_error(ErrorKind::InvalidTemplate))?;

        let context = self.ask_questions(&definition)?;

        if !output_dir.exists() {
            create_directory(&output_dir)?;
        }

        // Create the glob patterns first, only once
        let patterns: Vec<Pattern> = definition.copy_without_render
            .iter()
            .map(|s| Pattern::new(s).unwrap())
            .collect();

        // And now generate the files in the output dir given
        let walker = WalkDir::new(&self.path)
            .into_iter()
            .filter_entry(|e| !is_vcs(e))
            .filter_map(|e| e.ok());

        'outer: for entry in walker {
            // Skip root folder and the template.toml
            if entry.path() == self.path || entry.path() == conf_path {
                continue;
            }

            let path = entry.path().strip_prefix(&self.path).unwrap();
            let path_str = format!("{}", path.display());
            for ignored in &definition.ignore {
                if ignored == &path_str || path_str.starts_with(ignored) {
                    continue 'outer;
                }
            }

            let tpl = Tera::one_off(&path_str, &context, false)
                .map_err(|err| new_error(ErrorKind::Tera { err, path: path.to_path_buf() }))?;

            let real_path = output_dir.join(Path::new(&tpl));

            if entry.path().is_dir() {
                create_directory(&real_path)?;
                continue;
            }

            // Only pass non-binary files or the files not matching the copy_without_render patterns through Tera
            let mut f = File::open(&entry.path())?;
            let mut buffer = Vec::new();
            f.read_to_end(&mut buffer)?;

            let no_render = patterns.iter().map(|p| p.matches_path(&real_path)).any(|x| x);

            if no_render || is_binary(&buffer) {
                fs::copy(&entry.path(), &real_path)
                    .map_err(|err| new_error(ErrorKind::Io { err, path: entry.path().to_path_buf() }))?;
                continue;
            }

            let contents = Tera::one_off(&str::from_utf8(&buffer).unwrap(), &context, false)
                .map_err(|err| new_error(ErrorKind::Tera {err, path: entry.path().to_path_buf()}))?;
            write_file(&real_path, &contents)?;
        }

        println!("Everything done, ready to go!");

        Ok(())
    }
}
