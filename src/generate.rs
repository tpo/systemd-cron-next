use std::io::{self, Write};
use std::fmt::Display;
use std::fs::{PathExt, File, create_dir_all, set_permissions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::Path;
use std::collections::{BTreeMap, BTreeSet};

use cronparse::Limited;
use cronparse::crontab::{CrontabEntry, SystemCrontabEntry, UserCrontabEntry};
use cronparse::schedule::{Schedule, Period, Calendar};
use cronparse::interval::Interval;

use pgs_files::passwd;

use super::REBOOT_FILE;

pub fn generate_systemd_units(entry: CrontabEntry, env: &BTreeMap<String, String>, path: &Path, dstdir: &Path) -> io::Result<()> {
    use cronparse::crontab::CrontabEntry::*;

    info!("generating units for {}: \"{}\", {:?}", path.display(), entry, env);

    let owner = try!(path.metadata()).uid();

    let mut persistent = env.get("PERSISTENT").and_then(|v| match &**v {
        "yes" | "true" | "1" => Some(true),
        "auto" | "" => None,
        _ => Some(false)
    }).unwrap_or_else(|| match entry {
        Anacron(_) | User(UserCrontabEntry { sched: Schedule::Period(_), .. }) | System(SystemCrontabEntry { sched: Schedule::Period(_), .. }) => true,
        _ => false
    });

    let batch = env.get("BATCH").map(|v| match &**v {
        "yes" | "true" | "1" => true,
        _ => false
    }).unwrap_or(false);

    let random_delay = env.get("RANDOM_DELAY").and_then(|v| v.parse::<u64>().ok()).unwrap_or(1);
    let mut delay = env.get("DELAY").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let hour = env.get("START_HOURS_RANGE").and_then(|v| v.splitn(1, '-').next().and_then(|v| v.parse::<u64>().ok())).unwrap_or(0);
    let shell = env.get("SHELL").map(|v| &**v).unwrap_or("/bin/sh");
    let daemon_reload = Path::new(REBOOT_FILE).is_file();

    let schedule = entry.period().and_then(|period| match *period {
        Period::Reboot => {
            persistent = false;
            if delay == 0 {
                delay = 1;
            }
            None
        },
        Period::Minutely => {
            persistent = false;
            Some("@minutely".to_owned())
        },
        Period::Hourly => {
            if delay == 0 {
                Some("@hourly".to_owned())
            } else {
                Some(format!("*-*-* *:{}:0", delay))
            }
        },
        Period::Midnight => {
            if delay == 0 {
                Some("@daily".to_owned())
            } else {
                Some(format!("*-*-* 0:{}:0", delay))
            }
        },
        Period::Daily => {
            if delay == 0 && hour == 0 {
                Some("@daily".to_owned())
            } else {
                Some(format!("*-*-* {}:{}:0", hour, delay))
            }
        },
        Period::Weekly => {
            if delay == 0 && hour == 0 {
                Some("@weekly".to_owned())
            } else {
                Some(format!("Mon *-*-* {}:{}:0", hour, delay))
            }
        },
        Period::Monthly => {
            if delay == 0 && hour == 0 {
                Some("@monthly".to_owned())
            } else {
                Some(format!("*-*-1 {}:{}:0", hour, delay))
            }
        },
        Period::Quaterly => {
            if delay == 0 && hour == 0 {
                Some("@quaterly".to_owned())
            } else {
                Some(format!("*-1,4,7,10-1 {}:{}:0", hour, delay))
            }
        },
        Period::Biannually => {
            if delay == 0 && hour == 0 {
                Some("@semi-annually".to_owned())
            } else {
                Some(format!("*-1,7-1 {}:{}:0", hour, delay))
            }
        },
        Period::Yearly => {
            if delay == 0 && hour == 0 {
                Some("@yearly".to_owned())
            } else {
                Some(format!("*-1-1 {}:{}:0", hour, delay))
            }
        },
        Period::Days(days) => {
            // workaround for anacrontab
            if days > 31 {
                Some(format!("*-1/{}-1 {}:{}:0", days / 30, hour, delay))
            } else {
                Some(format!("*-*-1/{} {}:{}:0", days, hour, delay))
            }
        },
    }).or_else(|| entry.calendar().and_then(|cal| {
        let Calendar {
            ref dows,
            ref days,
            ref mons,
            ref hrs,
            ref mins
        } = *cal;

        Some(format!("{} *-{}-{} {}:{}:00",
                     linearize(&**dows, ""),
                     linearize(&**mons, "*"),
                     linearize(&**days, "*"),
                     linearize(&**hrs, "*"),
                     linearize(&**mins, "*")))
    }));

    if daemon_reload && schedule.is_none() {
        warn!("skipping job from {}: \"{}\"", path.display(), entry);
        return Ok(());
    }

    if let Some(cmd) = entry.command() {

        // make sure we know the user
        let user = try!(entry.user().and_then(passwd::get_entry_by_name).or_else(|| passwd::get_entry_by_uid(owner))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown user")));

        // generate unique cron job id
        let mut md5ctx = ::md5::Context::new();
        md5ctx.consume(path.as_os_str().as_bytes());
        if let Some(ref schedule) = schedule {
            md5ctx.consume(schedule.as_bytes());
        }
        md5ctx.consume(cmd.as_bytes());
        let md5hex = tohex(&md5ctx.compute());

        // create service and timer unit names
        let service_unit_name = format!("cronjob-{}.service", md5hex);
        let timer_unit_name = format!("cronjob-{}.timer", md5hex);

        // unit paths
        let service_unit_path = dstdir.join(&service_unit_name);
        let timer_unit_path = dstdir.join(&timer_unit_name);

        // make sure cron.target.wants dir exists
        let cron_target_wants_path = dstdir.join("cron.target.wants");
        try!(create_dir_all(&cron_target_wants_path));

        // process command in case it should be put into script
        let command = if Path::new(cmd).is_file() {
            cmd.to_owned()
        } else {
            let script_command_path = dstdir.join(format!("cronjob-{}.sh", md5hex));

            debug!("generating script {:?} from {:?}", script_command_path, path);
            {
                let mut script_command_file = try!(File::create(&script_command_path));
                try!(writeln!(script_command_file, "#!{}", shell));
                try!(writeln!(script_command_file, "{}", cmd));
            }

            let mut perms = try!(script_command_path.metadata()).permissions();
            perms.set_mode(0o755);
            try!(set_permissions(&script_command_path, perms));
            script_command_path.to_str().unwrap().to_owned()
        };

        debug!("generating service {:?} from {:?}", service_unit_path, path);
        {
            let mut service_unit_file = try!(File::create(service_unit_path));

            try!(writeln!(service_unit_file, r###"[Unit]
Description=[Cron] "{entry}"
Documentation=man:systemd-crontab-generator(8)
RefuseManualStart=true
RefuseManualStop=true
SourcePath={source_crontab_path}"###,
                entry = entry,
                source_crontab_path = path.display(),
                ));

            if env.contains_key("MAILTO") {
                try!(writeln!(service_unit_file, "OnFailure=cron-failure@%i.service"));
            }

            try!(writeln!(service_unit_file, r###"
[Service]
Type=oneshot
IgnoreSIGPIPE=false
ExecStart={command}"###,
                command = command,
                ));

            if user.uid != 0 {
                try!(writeln!(service_unit_file, "User={}", user.name));
            }

            if let Some(group) = entry.group() {
                try!(writeln!(service_unit_file, "Group={}", group));
            }
            if batch {
                try!(writeln!(service_unit_file, "CPUSchedulingPolicy=idle"));
                try!(writeln!(service_unit_file, "IOSchedulingClass=idle"));
            }

            if !env.is_empty() {
                try!(write!(service_unit_file, "Environment="));
                for (name, value) in env.iter() {
                    try!(write!(service_unit_file, r#""{}={}""#, name, value));
                }
                try!(write!(service_unit_file, "\n"));
            }
        }

        debug!("generating timer {:?} from {:?}", timer_unit_path, path);
        {
            let mut timer_unit_file = try!(File::create(&timer_unit_path));

            try!(writeln!(timer_unit_file, r###"[Unit]
Description=[Timer] "{entry}"
Documentation=man:systemd-crontab-generator(8)
PartOf=cron.target
RefuseManualStart=true
RefuseManualStop=true
SourcePath={source_crontab_path}

[Timer]
Unit={service_unit_name}
Persistent={persistent}"###,
                entry = entry,
                source_crontab_path = path.display(),
                service_unit_name = service_unit_name,
                persistent = persistent,
                ));

            if let Some(schedule) = schedule {
                try!(writeln!(timer_unit_file, "OnCalendar={}", schedule));
            } else {
                try!(writeln!(timer_unit_file, "OnBootSec={}m", delay));
            }

            if random_delay != 1 {
                try!(writeln!(timer_unit_file, "AccuracySec={}m", random_delay));
            }
        }
        try!(symlink(timer_unit_path, cron_target_wants_path.join(timer_unit_name)));
    }

    Ok(())
}

fn linearize<T: Limited + Display>(input: &[Interval<T>], star: &str) -> String {
    if input.len() == 1 && input[0] == Interval::Full(1) {
        star.to_owned()
    } else {
        let mut output = String::new();
        for part in input.iter().flat_map(|v| v.iter()).collect::<BTreeSet<_>>().iter() {
            output.push_str(&*part.to_string());
            output.push(',');
        }
        output.pop();
        output
    }
}

fn tohex(input: &[u8]) -> String {
    #[inline]
    fn hex(d: u8) -> char {
        match d {
            0...9 => (d + 0x30) as char,
            10...15 => (d + 0x57) as char,
            _ => unreachable!("unexpected value: {}", d)
        }
    }

    let mut buf = String::with_capacity(32);
    for b in input.into_iter() {
        buf.push(hex(b >> 4));
        buf.push(hex(b & 0xf));
    }
    buf
}

