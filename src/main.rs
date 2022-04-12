use argh::FromArgs;
use lazy_static::lazy_static;
use librad::{
    collaborative_objects::{History, NewObjectSpec, ObjectId, TypeName},
    git::{identities::local, storage::Storage, Urn},
    profile::Profile,
};
use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    str::FromStr,
    io::Write,
};

/// Interact with radicle collaborative objects
#[derive(FromArgs, PartialEq, Debug)]
struct Args {
    /// the project to create the new object into
    #[argh(option)]
    project_urn: Urn,
    #[argh(subcommand)]
    command: Command,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum Command {
    Create(Create),
    Retrieve(Retrieve),
    AddComment(AddComment),
    List(List),
    ChangeGraph(ChangeGraph),
    AutomergeDoc(AutomergeDoc),
}

/// Create a new issue
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "create")]
struct Create {
    /// the title of the issue
    #[argh(option)]
    title: String,
    /// the description of the issue
    #[argh(option)]
    description: String,
}

/// Retrieve an issue
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "get")]
struct Retrieve {
    /// the ID of the issue
    #[argh(option)]
    issue_id: ObjectId,
}

/// Add a comment to an issue
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "add-comment")]
struct AddComment {
    /// the ID of the object
    #[argh(option)]
    issue_id: ObjectId,
    /// the comment to add
    #[argh(option)]
    comment: String,
}

/// List issues
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "list")]
struct List {}

/// Output graphviz formatted description of the change graph for an issue
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "changegraph")]
struct ChangeGraph {
    /// the ID of the issue
    #[argh(option)]
    issue_id: ObjectId,
}

/// Dump the validated automerge document
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "automerge-doc")]
struct AutomergeDoc {
    /// the ID of the issue
    #[argh(option)]
    issue_id: ObjectId,
}

const SCHEMA_JSON_BYTES: &[u8; 702] = std::include_bytes!("./schema.json");

lazy_static! {
    static ref TYPENAME: TypeName = FromStr::from_str("xyz.example.radicle.issue").unwrap();
    static ref SCHEMA_JSON: serde_json::Value =
        serde_json::from_slice(&SCHEMA_JSON_BYTES[..]).unwrap();
}

fn main() {
    tracing_subscriber::fmt::init();
    let args: Args = argh::from_env();
    let profile = Profile::load().unwrap();
    let sock = lnk_clib::keys::ssh::SshAuthSock::default();
    let signer = match lnk_clib::keys::ssh::signer(&profile, sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to load signer: {}", e);
            return;
        }
    };
    let paths = profile.paths();
    let storage = Storage::open(&paths, signer.clone()).unwrap();
    let local_id = local::default(&storage).unwrap().unwrap();


    match args.command {
        Command::Create(Create { title, description }) => {
            let store = storage.collaborative_objects(Some(paths.cob_cache_dir().to_path_buf()));
            let author = local_id.urn();
            store
                .create(
                    &local_id,
                    &args.project_urn,
                    NewObjectSpec {
                        message: Some("create issue".to_string()),
                        typename: TYPENAME.clone(),
                        schema_json: SCHEMA_JSON.clone(),
                        history: initial_doc(author, title, description),
                    },
                )
                .unwrap();
        }
        Command::Retrieve(Retrieve { issue_id }) => {
            let store = storage.collaborative_objects(Some(paths.cob_cache_dir().to_path_buf()));
            let object = store
                .retrieve(&args.project_urn, &TYPENAME, &issue_id)
                .unwrap();
            if let Some(object) = object {
                match evaluate_history(object.history()) {
                    Ok((mut frontend, _backend)) => {
                        println!(
                            "{}",
                            serde_json::to_string(&frontend.state().to_json()).unwrap()
                        );
                    },
                    Err(e) => {
                        eprintln!("error evaluating {}", e);
                        return;
                    }
                }
            } else {
                println!("No object found");
            }
        }
        Command::AddComment(AddComment { issue_id, comment }) => {
            let store = storage.collaborative_objects(Some(paths.cob_cache_dir().to_path_buf()));
            let object = store
                .retrieve(&args.project_urn, &TYPENAME, &issue_id)
                .unwrap();
            if let Some(object) = object {
                let (mut frontend, mut backend) = match evaluate_history(object.history()) {
                    Ok(it) => it,
                    Err(e) => {
                        eprintln!("error loading issue: {}", e);
                        return;
                    }
                };
                frontend.apply_patch(backend.get_patch().unwrap()).unwrap();
                let change = frontend
                    .change(Some("Add a comment".to_string()), |d| {
                        let comments =
                            d.value_at_path(&automerge::Path::root().key("comments"));
                        let num_comments =
                            if let Some(automerge::Value::List(comments)) = comments {
                                comments.len() as u32
                            } else {
                                eprintln!("invalid issue document");
                                return Ok(());
                            };
                        let author = local_id.urn();
                        let mut comment_map = HashMap::new();
                        comment_map.insert("author".into(), automerge::Value::Primitive(automerge::Primitive::Str(author.to_string().into())));
                        comment_map.insert("comment".into(), automerge::Value::Primitive(automerge::Primitive::Str(comment.into())));
                        let new_comment = automerge::Value::Map(comment_map);
                            
                        //let new_comment = serde_json::json!({
                            //"author": author.to_string(),
                            //"comment": comment,
                        //});
                        d.add_change(automerge::LocalChange::insert(
                            automerge::Path::root().key("comments").index(num_comments),
                            new_comment,
                        ))
                    })
                    .unwrap()
                    .1
                    .unwrap();
                let change: automerge::Change = change.into();
                backend.apply_changes(vec![change.clone()]).unwrap();
                let contents = librad::collaborative_objects::EntryContents::Automerge(change.raw_bytes().to_vec());
                store
                    .update(
                        &local_id,
                        &args.project_urn,
                        librad::collaborative_objects::UpdateObjectSpec{
                            typename: TYPENAME.clone(),
                            object_id: issue_id,
                            changes: contents,
                            message: Some("add comment".to_string()),
                        }
                    )
                    .unwrap();
                println!("Update complete");
            } else {
                println!("No object found");
            }
        }
        Command::List(List {}) => {
            let store = storage.collaborative_objects(Some(paths.cob_cache_dir().to_path_buf()));
            let objects = store
                .list(&args.project_urn, &TYPENAME)
                .unwrap();
            for object in objects {
                let issue: Result<Issue, _> = object.history().try_into();
                match issue {
                    Ok(issue) => {
                        println!("{}\t{}", object.id(), issue.title);
                    }
                    Err(e) => {
                        println!("{}: failed to deserialize: {}", object.id(), e);
                    }
                }
            }
        }
        Command::ChangeGraph(ChangeGraph { issue_id }) => {
            let store = storage.collaborative_objects(Some(paths.cob_cache_dir().to_path_buf()));
            println!(
                "{}",
                store
                    .changegraph_info_for_object(&args.project_urn, &TYPENAME, &issue_id)
                    .unwrap()
                    .unwrap()
                    .dotviz
            )
        }
        Command::AutomergeDoc(AutomergeDoc { issue_id }) => {
            let store = storage.collaborative_objects(Some(paths.cob_cache_dir().to_path_buf()));
            let object = store
                .retrieve(&args.project_urn, &TYPENAME, &issue_id)
                .unwrap();
            if let Some(object) = object {
                match evaluate_history(object.history()) {
                    Ok((_frontend, backend)) => {
                        let mut stdout = std::io::stdout();
                        stdout.write(backend.save().unwrap().as_slice()).unwrap();
                    },
                    Err(e) => {
                        eprintln!("error: {}", e);
                    }
                }
            } else {
                println!("No object found");
            }
        }
    }
}

fn initial_doc(author: Urn, title: String, description: String) -> librad::collaborative_objects::EntryContents {
    let mut frontend = automerge::Frontend::new();
    let (_, change) = frontend
        .change::<_, (), automerge::InvalidChangeRequest>(Some("create issue".to_string()), |d| {
            let init = serde_json::json!({
                "title": title,
                "description": description,
                "author": author.to_string(),
                "comments": [],
            });
            d.add_change(automerge::LocalChange::set(
                automerge::Path::root(),
                automerge::Value::from_json(&init),
            ))?;
            Ok(())
        })
        .unwrap();
    let mut backend = automerge::Backend::new();
    let (_, change) = backend.apply_local_change(change.unwrap()).unwrap();
    librad::collaborative_objects::EntryContents::Automerge(change.raw_bytes().to_vec())
}

#[derive(serde::Deserialize)]
pub struct Issue {
    pub title: String,
    pub description: String,
    pub comments: Vec<Comment>,
    pub author: String,
}

#[derive(serde::Deserialize)]
pub struct Comment {
    pub comment: String,
    pub author: String,
}

impl TryFrom<&History> for Issue {
    type Error = anyhow::Error;

    fn try_from(history: &History) -> Result<Self, Self::Error> {
        let (mut frontend, _backend) = evaluate_history(history)?;
        serde_json::from_value(frontend.state().to_json()).map_err(|e| e.into())
    }
}

fn evaluate_history(history: &librad::collaborative_objects::History) -> anyhow::Result<(automerge::Frontend, automerge::Backend)> {
    let backend = history.traverse(automerge::Backend::new(), |mut backend, entry| {
        eprintln!("loading entry");
        match entry.contents() {
            librad::collaborative_objects::EntryContents::Automerge(data) => {
                match automerge::Change::from_bytes(data.clone()) {
                    Ok(c) => {
                        backend.apply_changes(vec![c]).ok();
                    },
                    Err(e) => {
                        eprintln!("Error loading a change: {}", e);
                    }
                }
            }
        }
        std::ops::ControlFlow::Continue(backend)
    });
    let mut frontend = automerge::Frontend::new();
    frontend.apply_patch(backend.get_patch()?)?;
    Ok((frontend, backend))
}
