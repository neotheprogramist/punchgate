#!/bin/bash
set -euo pipefail

# ============================================================================
# create_user.sh — Create a new user with SSH key + passwordless sudo
# ============================================================================

SCRIPT_NAME="$(basename "$0")"
VERSION="1.0.0"

# Defaults
USERNAME=""
SSH_KEY=""
SHELL_PATH="$(getent passwd root | cut -d: -f7)"
GROUPS="sudo"
COPY_DOTFILES=true
DRY_RUN=false

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

usage() {
    cat <<EOF
Usage: $SCRIPT_NAME --user <username> --ssh-key <public_key> [OPTIONS]

Required:
  --user, -u          Username to create
  --ssh-key, -k       SSH public key (string or path to .pub file)

Optional:
  --shell, -s         Login shell (default: root's shell — $SHELL_PATH)
  --groups, -g        Comma-separated groups (default: sudo)
  --no-dotfiles       Skip copying root's dotfiles
  --dry-run           Print actions without executing
  --help, -h          Show this help
  --version, -v       Show version

Examples:
  $SCRIPT_NAME --user deploy --ssh-key "ssh-ed25519 AAAA..."
  $SCRIPT_NAME --user deploy --ssh-key ~/.ssh/id_ed25519.pub --groups sudo,docker
  $SCRIPT_NAME --user deploy --ssh-key "ssh-ed25519 AAAA..." --shell /bin/zsh --dry-run
EOF
    exit 0
}

parse_args() {
    [[ $# -eq 0 ]] && usage

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --user|-u)
                USERNAME="$2"; shift 2 ;;
            --ssh-key|-k)
                SSH_KEY="$2"; shift 2 ;;
            --shell|-s)
                SHELL_PATH="$2"; shift 2 ;;
            --groups|-g)
                GROUPS="$2"; shift 2 ;;
            --no-dotfiles)
                COPY_DOTFILES=false; shift ;;
            --dry-run)
                DRY_RUN=true; shift ;;
            --help|-h)
                usage ;;
            --version|-v)
                echo "$SCRIPT_NAME $VERSION"; exit 0 ;;
            *)
                log_error "Unknown argument: $1"
                echo "Run '$SCRIPT_NAME --help' for usage."
                exit 1 ;;
        esac
    done
}

validate() {
    # Must be root
    if [[ $EUID -ne 0 ]]; then
        log_error "This script must be run as root."
        exit 1
    fi

    # Username required
    if [[ -z "$USERNAME" ]]; then
        log_error "Missing required argument: --user"
        exit 1
    fi

    # Validate username format
    if ! [[ "$USERNAME" =~ ^[a-z_][a-z0-9_-]{0,31}$ ]]; then
        log_error "Invalid username '$USERNAME'. Use lowercase letters, digits, hyphens, underscores (max 32 chars)."
        exit 1
    fi

    # User must not already exist
    if id "$USERNAME" &>/dev/null; then
        log_error "User '$USERNAME' already exists."
        exit 1
    fi

    # SSH key required
    if [[ -z "$SSH_KEY" ]]; then
        log_error "Missing required argument: --ssh-key"
        exit 1
    fi

    # If SSH_KEY is a file path, read it
    if [[ -f "$SSH_KEY" ]]; then
        log_info "Reading SSH key from file: $SSH_KEY"
        SSH_KEY="$(cat "$SSH_KEY")"
    fi

    # Basic SSH key format validation
    if ! [[ "$SSH_KEY" =~ ^(ssh-(rsa|ed25519|dss)|ecdsa-sha2) ]]; then
        log_error "Invalid SSH public key format. Expected ssh-rsa, ssh-ed25519, ecdsa-sha2, or ssh-dss."
        exit 1
    fi

    # Validate shell exists
    if [[ ! -x "$SHELL_PATH" ]]; then
        log_error "Shell '$SHELL_PATH' does not exist or is not executable."
        exit 1
    fi

    # Validate groups exist
    IFS=',' read -ra GROUP_LIST <<< "$GROUPS"
    for group in "${GROUP_LIST[@]}"; do
        if ! getent group "$group" &>/dev/null; then
            log_error "Group '$group' does not exist."
            exit 1
        fi
    done
}

run() {
    if [[ "$DRY_RUN" == true ]]; then
        echo -e "${YELLOW}[DRY-RUN]${NC} $*"
    else
        "$@"
    fi
}

create_user() {
    log_info "Creating user '$USERNAME' (shell: $SHELL_PATH, groups: $GROUPS)"
    run useradd -m -s "$SHELL_PATH" -G "$GROUPS" "$USERNAME"

    log_info "Setting up SSH authorized_keys"
    local ssh_dir="/home/$USERNAME/.ssh"
    run install -d -m 700 -o "$USERNAME" -g "$USERNAME" "$ssh_dir"
    run install -m 600 -o "$USERNAME" -g "$USERNAME" /dev/null "$ssh_dir/authorized_keys"
    if [[ "$DRY_RUN" == false ]]; then
        echo "$SSH_KEY" > "$ssh_dir/authorized_keys"
    fi

    log_info "Locking password (SSH-key-only access)"
    run passwd -l "$USERNAME"

    log_info "Configuring passwordless sudo"
    local sudoers_file="/etc/sudoers.d/$USERNAME"
    if [[ "$DRY_RUN" == false ]]; then
        echo "$USERNAME ALL=(ALL) NOPASSWD:ALL" > "$sudoers_file"
        chmod 440 "$sudoers_file"
        # Validate sudoers syntax
        if ! visudo -cf "$sudoers_file" &>/dev/null; then
            log_error "Invalid sudoers syntax — removing $sudoers_file"
            rm -f "$sudoers_file"
            exit 1
        fi
    else
        run echo "$USERNAME ALL=(ALL) NOPASSWD:ALL > $sudoers_file"
    fi

    if [[ "$COPY_DOTFILES" == true ]]; then
        log_info "Copying dotfiles from root"
        for dotfile in .bashrc .bash_aliases .profile .vimrc; do
            if [[ -f "/root/$dotfile" ]]; then
                run cp "/root/$dotfile" "/home/$USERNAME/$dotfile"
            fi
        done
        if [[ "$DRY_RUN" == false ]]; then
            chown -R "$USERNAME":"$USERNAME" "/home/$USERNAME"
        fi
    fi
}

summary() {
    cat <<EOF

${GREEN}══════════════════════════════════════════════════${NC}
  User '${USERNAME}' created successfully
${GREEN}══════════════════════════════════════════════════${NC}
  Shell:            $SHELL_PATH
  Groups:           $GROUPS
  Home:             /home/$USERNAME
  SSH key:          installed
  Password:         locked (key-only)
  Sudo:             passwordless
  Dotfiles copied:  $COPY_DOTFILES
${GREEN}══════════════════════════════════════════════════${NC}

  Test with: ssh ${USERNAME}@<host>
EOF
}

# ============================================================================
# Main
# ============================================================================
parse_args "$@"
validate
create_user
[[ "$DRY_RUN" == false ]] && summary || log_warn "Dry run complete — no changes made."
