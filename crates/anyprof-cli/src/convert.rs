use std::path::Path;

use anyhow::{Context, anyhow};

use crate::cli::{ConvertArgs, ProfileFormat};

pub fn run(args: ConvertArgs) -> anyhow::Result<()> {
    let from = match args.from {
        Some(format) => format,
        None => infer_format(&args.input).ok_or_else(|| {
            anyhow!(
                "could not infer the input format from {} (pass --from)",
                args.input.display()
            )
        })?,
    };
    let bytes =
        std::fs::read(&args.input).with_context(|| format!("reading {}", args.input.display()))?;
    let profile = anyprof_export::import(&bytes, from.into())?;
    let output = anyprof_export::export(&profile, args.to.into())?;
    std::fs::write(&args.output, output)
        .with_context(|| format!("writing {}", args.output.display()))?;
    eprintln!(
        "converted {} ({}) -> {} ({})",
        args.input.display(),
        from.as_str(),
        args.output.display(),
        args.to.as_str()
    );
    Ok(())
}

fn infer_format(path: &Path) -> Option<ProfileFormat> {
    crate::cli::infer_format(path)
}

#[cfg(test)]
mod tests {
    use super::infer_format;
    use crate::cli::ProfileFormat;
    use std::path::Path;

    #[test]
    fn infers_known_extensions() {
        assert_eq!(
            infer_format(Path::new("cpu.collapsed")),
            Some(ProfileFormat::Collapsed)
        );
        assert_eq!(
            infer_format(Path::new("cpu.folded")),
            Some(ProfileFormat::Collapsed)
        );
        assert_eq!(
            infer_format(Path::new("stacks.txt")),
            Some(ProfileFormat::Collapsed)
        );
        assert_eq!(
            infer_format(Path::new("cpu.json")),
            Some(ProfileFormat::Speedscope)
        );
        assert_eq!(
            infer_format(Path::new("cpu.speedscope.json")),
            Some(ProfileFormat::Speedscope)
        );
        assert_eq!(
            infer_format(Path::new("cpu.pb.gz")),
            Some(ProfileFormat::Pprof)
        );
        assert_eq!(
            infer_format(Path::new("profile.cpuprofile")),
            Some(ProfileFormat::Cpuprofile)
        );
        assert_eq!(infer_format(Path::new("cpu.gz")), None);
    }
}
