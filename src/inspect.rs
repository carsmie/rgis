//! `rgis --inspect`: print how a project is understood.

use crate::project::{Project, TreeGroup, TreeNode};

fn print_tree(p: &Project, g: &TreeGroup, depth: usize) {
    for c in &g.children {
        let ind = "  ".repeat(depth);
        match c {
            TreeNode::Group(sub) => {
                println!("{ind}[group] {} (WMS name '{}')", sub.name, sub.nickname());
                print_tree(p, sub, depth + 1);
            }
            TreeNode::Layer { id, .. } => match p.layers.get(id) {
                Some(l) => {
                    let provider = match l.provider() {
                        Ok(pr) => format!("{} ({} fields)", pr.description, pr.fields.len()),
                        Err(e) => format!("INVALID: {e:#}"),
                    };
                    println!(
                        "{ind}[layer] {} (WMS name '{}', {}, {})",
                        l.name,
                        p.nickname(l),
                        l.geometry,
                        l.crs().authid
                    );
                    println!("{ind}        source:   {}", l.datasource);
                    println!("{ind}        provider: {provider}");
                    println!(
                        "{ind}        renderer: {} legend item(s)",
                        l.renderer.legend().len()
                    );
                }
                None => println!("{ind}[layer] {id} (missing from <projectlayers>)"),
            },
        }
    }
}

/// Print a project's services, CRSs and layer tree with each layer's provider status.
pub fn inspect(path: &str) -> anyhow::Result<()> {
    let p = Project::read(std::path::Path::new(path))?;
    println!("Project:   {}", p.path.display());
    println!("Title:     {}", p.title);
    println!("CRS:       {}", p.crs.authid);
    println!("WMS root:  {}", p.root_name());
    println!("CRS list:  {}", p.crs_list().join(", "));
    println!(
        "WFS layers: {}",
        p.wfs_layers()
            .iter()
            .map(|l| p.nickname(l))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("Layer tree:");
    print_tree(&p, &p.tree, 1);
    Ok(())
}
