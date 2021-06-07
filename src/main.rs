use anyhow::anyhow;
use argh::FromArgs;
use lazy_static::lazy_static;
use librad::{
    collaborative_objects::{History, NewObjectSpec, ObjectId, TypeName},
    git::{identities::local, storage::Storage, Urn},
    keys::{PublicKey, SecretKey},
    profile::Profile,
    signer::{BoxedSigner, SomeSigner},
};
use radicle_keystore::{
    crypto::{self, Pwhash},
    pinentry::Prompt,
    FileStorage, Keystore,
};
use std::{
    convert::{TryFrom, TryInto},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
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

const SCHEMA_JSON_BYTES: &[u8; 607] = std::include_bytes!("./schema.json");

lazy_static! {
    static ref TYPENAME: TypeName = FromStr::from_str("xyz.radicle.issue").unwrap();
    static ref SCHEMA_JSON: serde_json::Value =
        serde_json::from_slice(&SCHEMA_JSON_BYTES[..]).unwrap();
}

fn main() {
    tracing_subscriber::fmt::init();
    let args: Args = argh::from_env();
    let profile = Profile::load().unwrap();
    let paths = profile.paths();
    let signer = get_signer(paths.keys_dir()).unwrap();
    let storage = Storage::open(&paths, signer.clone()).unwrap();
    let local_id = local::default(&storage).unwrap().unwrap();

    match args.command {
        Command::Create(Create { title, description }) => {
            let store = storage.collaborative_objects();
            let author = local_id.urn();
            store
                .create_object(NewObjectSpec {
                    message: Some("create issue".to_string()),
                    typename: TYPENAME.to_string(),
                    project_urn: args.project_urn,
                    schema_json: SCHEMA_JSON.clone(),
                    history: initial_doc(author, title, description),
                })
                .unwrap();
        }
        Command::Retrieve(Retrieve { issue_id }) => {
            let store = storage.collaborative_objects();
            let object = store
                .retrieve_object(&args.project_urn, &TYPENAME, &issue_id)
                .unwrap();
            if let Some(object) = object {
                match object.history() {
                    History::Automerge(bytes) => {
                        let backend = automerge::Backend::load(bytes.clone()).unwrap();
                        let mut frontend = automerge::Frontend::new();
                        frontend.apply_patch(backend.get_patch().unwrap()).unwrap();
                        println!(
                            "{}",
                            serde_json::to_string(&frontend.state().to_json()).unwrap()
                        );
                    }
                }
            } else {
                println!("No object found");
            }
        }
        Command::AddComment(AddComment { issue_id, comment }) => {
            let store = storage.collaborative_objects();
            let object = store
                .retrieve_object(&args.project_urn, &TYPENAME, &issue_id)
                .unwrap();
            if let Some(object) = object {
                match object.history() {
                    History::Automerge(bytes) => {
                        let mut backend = automerge::Backend::load(bytes.clone()).unwrap();
                        let mut frontend = automerge::Frontend::new();
                        frontend.apply_patch(backend.get_patch().unwrap()).unwrap();
                        let change = frontend
                            .change(Some("Add a comment".to_string()), |d| {
                                let comments =
                                    d.value_at_path(&automerge::Path::root().key("comments"));
                                let num_comments =
                                    if let Some(automerge::Value::Sequence(comments)) = comments {
                                        comments.len() as u32
                                    } else {
                                        println!("invalid issue document");
                                        return Ok(());
                                    };
                                let author = local_id.urn();
                                let new_comment = serde_json::json!({
                                    "author": author.to_string(),
                                    "comment": comment,
                                });
                                d.add_change(automerge::LocalChange::insert(
                                    automerge::Path::root().key("comments").index(num_comments),
                                    automerge::Value::from_json(&new_comment),
                                ))
                            })
                            .unwrap()
                            .1
                            .unwrap();
                        let change: automerge::Change = change.into();
                        backend.apply_changes(vec![change.clone()]).unwrap();
                        store
                            .update_object(
                                &args.project_urn,
                                &TYPENAME,
                                &issue_id,
                                History::Automerge(change.raw_bytes().to_vec()),
                                Some("add comment".to_string()),
                            )
                            .unwrap();
                        println!("Update complete");
                    }
                }
            } else {
                println!("No object found");
            }
        }
        Command::List(List {}) => {
            let store = storage.collaborative_objects();
            let objects = store
                .retrieve_objects(&args.project_urn, &TYPENAME)
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
            let store = storage.collaborative_objects();
            println!(
                "{}",
                store
                    .changegraph_dotviz_for_object(&args.project_urn, &TYPENAME, &issue_id)
                    .unwrap()
            )
        }
    }
}

fn get_signer(keys_dir: &Path) -> anyhow::Result<BoxedSigner> {
    let file = default_signer_file(keys_dir)?;
    let keystore = FileStorage::<_, PublicKey, _, _>::new(
        &file,
        Pwhash::new(
            Prompt::new("please enter your Radicle password: "),
            *crypto::KDF_PARAMS_PROD,
        ),
    );
    let key: SecretKey = keystore.get_key().map(|keypair| keypair.secret_key)?;

    Ok(SomeSigner { signer: key }.into())
}

fn default_signer_file(keys_dir: &Path) -> anyhow::Result<PathBuf> {
    let mut keys = fs::read_dir(keys_dir)?;
    match keys.next() {
        None => Err(anyhow!(
            "No key was found in `{}`, have you initialised your key yet?",
            keys_dir.display()
        )),
        Some(key) => {
            if keys.next().is_some() {
                Err(anyhow!("Multiple keys were found in `{}`, you will have to specify which key you are using", keys_dir.display()))
            } else {
                Ok(key?.path())
            }
        }
    }
}

fn initial_doc(author: Urn, title: String, description: String) -> History {
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
    History::Automerge(change.raw_bytes().to_vec())
}

#[derive(serde::Deserialize)]
struct Issue {
    title: String,
    description: String,
    comments: Vec<Comment>,
    author: String,
}

#[derive(serde::Deserialize)]
struct Comment {
    comment: String,
    author: String,
}

impl TryFrom<&History> for Issue {
    type Error = anyhow::Error;

    fn try_from(value: &History) -> Result<Self, Self::Error> {
        match value {
            History::Automerge(bytes) => {
                let backend = automerge::Backend::load(bytes.clone())?;
                let mut frontend = automerge::Frontend::new();
                let patch = backend.get_patch()?;
                frontend.apply_patch(patch)?;
                serde_json::from_value(frontend.state().to_json()).map_err(|e| e.into())
            }
        }
    }
}
