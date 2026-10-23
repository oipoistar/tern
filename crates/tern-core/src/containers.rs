//! Containers running on a host, through its Docker or Podman: listed by
//! a command run beside the terminal, and a shell inside one opened as the
//! command of a pane's session.

/// Lists the running containers, one per line as `tool|id|name|image`,
/// with Docker, else Podman. Fails (status 3) on a host with neither.
pub const LIST: &str = "sh -c 'for tool in docker podman; do \
     if command -v $tool >/dev/null 2>&1; then \
     exec $tool ps --format \"$tool|{{.ID}}|{{.Names}}|{{.Image}}\"; fi; done; exit 3'";

/// A container running on a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    /// What runs it: `docker` or `podman`.
    pub tool: String,
    pub id: String,
    pub name: String,
    pub image: String,
}

/// The containers [`LIST`] printed. Lines that aren't one are skipped.
pub fn parse(listed: &str) -> Vec<Container> {
    listed
        .lines()
        .filter_map(|line| {
            let mut fields = line.trim().splitn(4, '|');
            let tool = fields.next()?;
            if tool != "docker" && tool != "podman" {
                return None;
            }
            let (id, name, image) = (fields.next()?, fields.next()?, fields.next()?);
            let safe = |text: &str| {
                !text.is_empty()
                    && text
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
            };
            // A name or id is all a shell command will carry.
            (safe(id) && safe(name)).then(|| Container {
                tool: tool.into(),
                id: id.into(),
                name: name.into(),
                image: image.into(),
            })
        })
        .collect()
}

/// The command that opens a shell inside `container`: bash where it has
/// it, else sh.
pub fn shell(container: &Container) -> String {
    format!(
        "{} exec -it {} sh -c \"command -v bash >/dev/null && exec bash || exec sh\"",
        container.tool, container.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_listing() {
        let listed = "docker|3f2a9c1b|web|nginx:1.27\n\
                      docker|77aa|db_1|postgres@sha256:abc|x\n\
                      podman|9e|cache|docker.io/library/redis:7\n\
                      Got permission denied while trying to connect\n\
                      docker|bad id|name|image\n\
                      docker|1a|evil;rm -rf|image\n";
        let containers = parse(listed);
        let names: Vec<&str> = containers.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["web", "db_1", "cache"]);
        // An image keeps whatever follows.
        assert_eq!(containers[1].image, "postgres@sha256:abc|x");
        assert_eq!(containers[2].tool, "podman");
        assert!(parse("").is_empty());
    }

    #[test]
    fn opens_a_shell_with_the_tool_that_listed_it() {
        let container = Container {
            tool: "podman".into(),
            id: "9e".into(),
            name: "cache".into(),
            image: "redis".into(),
        };
        let command = shell(&container);
        assert!(command.starts_with("podman exec -it cache sh -c \""));
        assert!(command.contains("exec bash || exec sh"));
        assert!(LIST.contains("docker podman") && LIST.contains("exit 3"));
    }
}
