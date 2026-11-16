-- VmaX identity, presence, friends and invite links.
--
-- Apply with: supabase db push   (or paste into the SQL editor).
-- Every table is protected by RLS: a row is visible only to the account that
-- owns it, or to the other side of an accepted friendship.

create extension if not exists "pgcrypto";

-- ---------------------------------------------------------------------------
-- profiles
-- ---------------------------------------------------------------------------
create table if not exists public.profiles (
  id uuid primary key default gen_random_uuid(),
  username text not null check (char_length(username) between 3 and 24),
  tag text not null check (tag ~ '^[0-9]{4}$'),
  avatar_url text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (username, tag)
);

comment on table public.profiles is
  'Public identity for a VmaX account. `username#tag` is the searchable handle.';

-- ---------------------------------------------------------------------------
-- devices
-- ---------------------------------------------------------------------------
create table if not exists public.devices (
  device_id uuid primary key,
  owner_id uuid not null references public.profiles(id) on delete cascade,
  device_name text not null,
  platform text not null check (platform in ('windows','macos','linux','android','ios')),
  public_key text not null,
  can_host boolean not null default false,
  last_seen timestamptz not null default now(),
  created_at timestamptz not null default now(),
  unique (owner_id, device_id)
);

create index if not exists devices_owner_idx on public.devices (owner_id);

-- ---------------------------------------------------------------------------
-- friendships
-- ---------------------------------------------------------------------------
create table if not exists public.friendships (
  id uuid primary key default gen_random_uuid(),
  requester_id uuid not null references public.profiles(id) on delete cascade,
  addressee_id uuid not null references public.profiles(id) on delete cascade,
  status text not null default 'pending' check (status in ('pending','accepted','blocked')),
  auto_approve_join boolean not null default false,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  check (requester_id <> addressee_id)
);

-- One row per pair regardless of direction.
create unique index if not exists friendships_pair_idx
  on public.friendships (least(requester_id, addressee_id), greatest(requester_id, addressee_id));
create index if not exists friendships_addressee_idx on public.friendships (addressee_id);

-- Only the addressee may flip auto_approve_join; the requester must not be able
-- to grant themselves unattended access to a machine.
create or replace function public.friendships_guard()
returns trigger
language plpgsql
security definer
set search_path = public
as $$
begin
  if new.requester_id <> old.requester_id or new.addressee_id <> old.addressee_id then
    raise exception 'friendship parties are immutable';
  end if;
  if new.auto_approve_join <> old.auto_approve_join
     and auth.uid() <> new.addressee_id then
    raise exception 'only the addressee can change auto_approve_join';
  end if;
  new.updated_at := now();
  return new;
end;
$$;

drop trigger if exists friendships_guard_trg on public.friendships;
create trigger friendships_guard_trg
  before update on public.friendships
  for each row execute function public.friendships_guard();

-- ---------------------------------------------------------------------------
-- session_invites
-- ---------------------------------------------------------------------------
create table if not exists public.session_invites (
  token text primary key,
  host_user_id uuid not null references public.profiles(id) on delete cascade,
  host_device_id uuid not null references public.devices(device_id) on delete cascade,
  max_uses int not null default 1 check (max_uses > 0),
  uses_count int not null default 0 check (uses_count >= 0),
  expires_at timestamptz not null default (now() + interval '24 hours'),
  created_at timestamptz not null default now(),
  check (uses_count <= max_uses)
);

create index if not exists session_invites_host_idx on public.session_invites (host_user_id);

-- ---------------------------------------------------------------------------
-- RLS
-- ---------------------------------------------------------------------------
alter table public.profiles enable row level security;
alter table public.devices enable row level security;
alter table public.friendships enable row level security;
alter table public.session_invites enable row level security;

-- profiles: readable by the owner and by friends (either direction, any status
-- except blocked). Writes are limited to the owner.
drop policy if exists profiles_read on public.profiles;
create policy profiles_read on public.profiles
  for select using (
    id = auth.uid()
    or exists (
      select 1 from public.friendships f
      where f.status <> 'blocked'
        and ((f.requester_id = auth.uid() and f.addressee_id = profiles.id)
          or (f.addressee_id = auth.uid() and f.requester_id = profiles.id))
    )
  );

drop policy if exists profiles_write on public.profiles;
create policy profiles_write on public.profiles
  for update using (id = auth.uid()) with check (id = auth.uid());

drop policy if exists profiles_insert on public.profiles;
create policy profiles_insert on public.profiles
  for insert with check (id = auth.uid());

-- devices: each owner sees their own; accepted friends see hostable devices so
-- the Computers tab can list "INSTANT SELF-CONNECT" targets on the same account
-- and friends can reach a machine.
drop policy if exists devices_own on public.devices;
create policy devices_own on public.devices
  for all using (owner_id = auth.uid()) with check (owner_id = auth.uid());

drop policy if exists devices_friends_read on public.devices;
create policy devices_friends_read on public.devices
  for select using (
    owner_id <> auth.uid()
    and exists (
      select 1 from public.friendships f
      where f.status = 'accepted'
        and ((f.requester_id = auth.uid() and f.addressee_id = devices.owner_id)
          or (f.addressee_id = auth.uid() and f.requester_id = devices.owner_id))
    )
  );

-- friendships: both parties read; requester inserts the pending row; addressee
-- accepts/blocks; either side can delete.
drop policy if exists friendships_read on public.friendships;
create policy friendships_read on public.friendships
  for select using (requester_id = auth.uid() or addressee_id = auth.uid());

drop policy if exists friendships_insert on public.friendships;
create policy friendships_insert on public.friendships
  for insert with check (requester_id = auth.uid());

drop policy if exists friendships_update on public.friendships;
create policy friendships_update on public.friendships
  for update using (requester_id = auth.uid() or addressee_id = auth.uid());

drop policy if exists friendships_delete on public.friendships;
create policy friendships_delete on public.friendships
  for delete using (requester_id = auth.uid() or addressee_id = auth.uid());

-- invites: the host sees and manages their own tokens. Redemption goes through
-- the RPC below so uses_count cannot be forged by the client.
drop policy if exists session_invites_own on public.session_invites;
create policy session_invites_own on public.session_invites
  for all using (host_user_id = auth.uid()) with check (host_user_id = auth.uid());

-- ---------------------------------------------------------------------------
-- Invite redemption
-- ---------------------------------------------------------------------------
-- Atomically consumes one use. Returns the host + device to dial, or no rows if
-- the token is unknown, expired or exhausted. Deliberately does not return the
-- token owner's secrets, only the routing information.
create or replace function public.redeem_session_invite(p_token text)
returns table (host_user_id uuid, host_device_id uuid, can_host boolean)
language plpgsql
security definer
set search_path = public
as $$
begin
  return query
  with consumed as (
    update public.session_invites i
       set uses_count = i.uses_count + 1
     where i.token = p_token
       and i.expires_at > now()
       and i.uses_count < i.max_uses
    returning i.host_user_id, i.host_device_id
  )
  select c.host_user_id, c.host_device_id, d.can_host
    from consumed c
    join public.devices d on d.device_id = c.host_device_id;
end;
$$;

revoke all on function public.redeem_session_invite(text) from public;
grant execute on function public.redeem_session_invite(text) to authenticated;

-- ---------------------------------------------------------------------------
-- Presence (broadcast, not persisted)
-- ---------------------------------------------------------------------------
-- Presence rides Supabase Realtime broadcast on `signal:device:<device_id>`
-- and `presence:user:<user_id>`; nothing is stored server-side. RLS above still
-- governs who may join a room: policies are only needed for the tables.
do $$
begin
  alter publication supabase_realtime add table public.friendships;
exception when duplicate_object then null;
end $$;
