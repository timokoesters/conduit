# Prerequisites for running Conduit

You'll need:

- A domain. Commonly cost about $10/year.
- A Linux server with a stable internet connection, at least 500 MB of RAM and some disk space for messages and
  attachments. Commonly start at $5/month.
- Some basic knowledge about using a shell, SSH and configuring and protecting a server.
  
  
## A word of caution:

Don't underestimate the toll of administrating your own server.
Conduit can't protect your conversations if your server gets compromised or deleted.

Make sure that you got:

- Automatic security updates
    - On Ubuntu/Debian: Set up unattended-upgrades
    - On RHEL/CentOS: Have a look at yum-cron
- A firewall blocking all but the needed incoming ports
    - ufw is an easy interface for the linux firewall
- Protection against automatic attacks
    - fail2ban scans logs and bans IPs which try to brute force their way into your server.
    - Disable ssh login for root and switch from password to key based authentication.
- Automated backups
    - Most VPS hosting companies offer whole server backups for a small fee.
    - Or run your own backup with something like borg.
- A way to get notified if your disk fills up.
    - If you send too much cat videos to your friends, Conduit might at some point become unable to
      store any important messages.