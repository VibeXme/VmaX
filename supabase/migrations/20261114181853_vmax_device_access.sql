-- Auth-linked identities and capability-scoped device dialing.
alter table public.profiles alter column id drop default;
alter table public.profiles add constraint profiles_auth_owner
  foreign key (id) references auth.users(id) on delete cascade not valid;
create unique index profiles_handle_ci on public.profiles(lower(username), tag);

alter table public.devices add column status text not null default 'online'
  check (status in ('online', 'hosting', 'in_game'));
alter table public.devices add column active_app text;
alter table public.devices add column tls_fingerprint text
  check (tls_fingerprint ~ '^[0-9a-f]{64}$');
alter table public.devices add constraint device_ed25519_key
  check (public_key ~ '^[0-9a-f]{64}$') not valid;

drop policy friendships_insert on public.friendships;
create policy friendships_insert on public.friendships for insert to authenticated
  with check (requester_id = (select auth.uid()) and status = 'pending' and not auto_approve_join);
drop policy friendships_update on public.friendships;
create policy friendships_update on public.friendships for update to authenticated
  using ((select auth.uid()) in (requester_id, addressee_id))
  with check ((select auth.uid()) in (requester_id, addressee_id));
alter table public.friendships add column requester_auto_approve boolean not null default false;
create index friendships_requester_idx on public.friendships (requester_id);
create or replace function public.friendships_guard()
returns trigger language plpgsql set search_path = '' as $$
begin
  if (new.id, new.requester_id, new.addressee_id) is distinct from
     (old.id, old.requester_id, old.addressee_id) then
    raise exception 'friendship identity is immutable';
  end if;
  if new.status is distinct from old.status and not (
     new.status = 'blocked' or
     (old.status = 'pending' and new.status = 'accepted' and auth.uid() = old.addressee_id)
  ) then raise exception 'invalid friendship transition'; end if;
  if new.auto_approve_join is distinct from old.auto_approve_join
     and auth.uid() is distinct from old.addressee_id then raise exception 'wrong approval owner'; end if;
  if new.requester_auto_approve is distinct from old.requester_auto_approve
     and auth.uid() is distinct from old.requester_id then raise exception 'wrong approval owner'; end if;
  if new.status <> 'accepted' then
    new.auto_approve_join := false;
    new.requester_auto_approve := false;
  end if;
  new.updated_at := now();
  return new;
end $$;
create or replace function public.friendships_insert_guard()
returns trigger language plpgsql set search_path = '' as $$
begin
  if new.requester_auto_approve then raise exception 'friendship must be accepted first'; end if;
  return new;
end $$;
create trigger friendships_insert_guard before insert on public.friendships
  for each row execute function public.friendships_insert_guard();
drop policy friendships_delete on public.friendships;
create policy friendships_delete on public.friendships for delete to authenticated
  using ((select auth.uid()) in (requester_id, addressee_id) and status <> 'blocked');

drop policy session_invites_own on public.session_invites;
create policy session_invites_read on public.session_invites for select to authenticated
  using (host_user_id = (select auth.uid()));
create policy session_invites_new on public.session_invites for insert to authenticated
  with check (host_user_id = (select auth.uid()) and uses_count = 0 and
    token ~ '^[0-9a-f]{64}$' and max_uses between 1 and 20 and
    expires_at <= now() + interval '1 day' and expires_at > now());
create policy session_invites_revoke on public.session_invites for delete to authenticated
  using (host_user_id = (select auth.uid()));
alter table public.session_invites add constraint invite_device_owner
  foreign key (host_user_id, host_device_id) references public.devices(owner_id, device_id)
  on delete cascade not valid;
drop function public.redeem_session_invite(text);

create table public.connect_requests (
  id uuid primary key,
  requester_id uuid not null references public.profiles on delete cascade,
  source_device uuid not null,
  host_user_id uuid not null references public.profiles on delete cascade,
  target_device uuid not null,
  source_public_key text not null,
  source_tls_fingerprint text not null,
  source_label text not null,
  signature text not null check (signature ~ '^[0-9a-f]{128}$'),
  expires_at_ms bigint not null,
  status text not null default 'pending' check (status in ('pending', 'approved', 'declined')),
  route text,
  created_at timestamptz not null default now(),
  foreign key (requester_id, source_device) references public.devices(owner_id, device_id) on delete cascade,
  foreign key (host_user_id, target_device) references public.devices(owner_id, device_id) on delete cascade
);
alter table public.connect_requests enable row level security;
create index connect_requester_created on public.connect_requests(requester_id, created_at);
create index connect_target_pending on public.connect_requests(target_device, expires_at_ms) where status = 'pending';
create policy request_participants on public.connect_requests for select to authenticated
  using ((select auth.uid()) in (requester_id, host_user_id));
create policy request_host_decision on public.connect_requests for update to authenticated
  using (host_user_id = (select auth.uid()) and status = 'pending'
    and expires_at_ms > floor(extract(epoch from now()) * 1000))
  with check (host_user_id = (select auth.uid()) and
    ((status = 'approved' and route like 'vmax://join/%' and length(route) <= 2048)
      or (status = 'declined' and route is null)));

grant select, insert, update on public.profiles to authenticated;
grant select, insert, update, delete on public.devices, public.friendships to authenticated;
revoke all on public.session_invites, public.connect_requests from public, anon, authenticated;
grant select, delete on public.session_invites to authenticated;
grant insert (token, host_user_id, host_device_id, max_uses, expires_at) on public.session_invites to authenticated;
grant select on public.connect_requests to authenticated;
grant update (status, route) on public.connect_requests to authenticated;

-- Realtime clients can subscribe to their device topic. All request writes go
-- through the database so clients cannot publish forged signaling events.
create policy vmax_device_realtime on realtime.messages for select to authenticated
  using (extension = 'broadcast' and exists (
    select 1 from public.devices d where d.owner_id = (select auth.uid())
      and (select realtime.topic()) = 'signal:device:' || d.device_id::text
  ));
create function public.vmax_touch_device()
returns trigger language plpgsql set search_path = '' as $$
begin
  if tg_op = 'UPDATE' and (old.owner_id, old.device_id, old.public_key, old.tls_fingerprint)
      is distinct from (new.owner_id, new.device_id, new.public_key, new.tls_fingerprint)
      and old.tls_fingerprint is not null then
    raise exception 'Registered device keys are immutable';
  end if;
  if new.last_seen > '1970-01-02T00:00:00Z' then new.last_seen := now(); end if;
  return new;
end $$;
create trigger vmax_touch_device before insert or update on public.devices
  for each row execute function public.vmax_touch_device();

create schema if not exists vmax_private;
grant usage on schema vmax_private to authenticated;
create function vmax_private.find_profile(p_username text, p_tag text)
returns setof public.profiles language sql stable security definer set search_path = '' as $$
  select p.* from public.profiles p
  where auth.uid() is not null and lower(p.username) = lower(p_username) and p.tag = p_tag
  and not exists (select 1 from public.friendships f where f.status = 'blocked'
    and least(f.requester_id, f.addressee_id) = least(auth.uid(), p.id)
    and greatest(f.requester_id, f.addressee_id) = greatest(auth.uid(), p.id));
$$;
create function public.find_vmax_profile(p_username text, p_tag text)
returns setof public.profiles language sql stable security invoker set search_path = '' as $$
  select * from vmax_private.find_profile(p_username, p_tag)
$$;

create function vmax_private.request_connect(p_id uuid, p_source uuid, p_target uuid,
  p_expiry bigint, p_signature text, p_token text)
returns uuid language plpgsql security definer set search_path = '' as $$
declare
  caller uuid := auth.uid();
  origin public.devices;
  host public.devices;
  instant bigint := floor(extract(epoch from clock_timestamp()) * 1000);
begin
  if caller is null or p_expiry not between instant + 1000 and instant + 90000 or p_source = p_target
      or p_signature !~ '^[0-9a-f]{128}$' then raise exception 'invalid request'; end if;
  perform pg_advisory_xact_lock(hashtextextended(caller::text, 0));
  if (select count(*) from public.connect_requests where requester_id = caller and created_at > now() - interval '1 minute') >= 12 then
    raise exception 'too many requests';
  end if;
  select * into strict origin from public.devices where device_id = p_source and owner_id = caller
    and tls_fingerprint is not null;
  select * into strict host from public.devices where device_id = p_target and can_host
    and last_seen > now() - interval '60 seconds' and tls_fingerprint is not null;
  if exists (select 1 from public.friendships where status = 'blocked'
      and least(requester_id, addressee_id) = least(caller, host.owner_id)
      and greatest(requester_id, addressee_id) = greatest(caller, host.owner_id)) then
    raise exception 'device unavailable';
  end if;
  if p_token is not null then
    update public.session_invites set uses_count = uses_count + 1
      where token = p_token and host_device_id = p_target and host_user_id = host.owner_id
      and expires_at > now() and uses_count < max_uses;
    if not found then raise exception 'invalid, expired, or exhausted invite'; end if;
  elsif caller <> host.owner_id and not exists (
    select 1 from public.friendships where status = 'accepted'
      and least(requester_id, addressee_id) = least(caller, host.owner_id)
      and greatest(requester_id, addressee_id) = greatest(caller, host.owner_id)
  ) then raise exception 'device unavailable'; end if;
  insert into public.connect_requests
    (id, requester_id, source_device, host_user_id, target_device, source_public_key,
     source_tls_fingerprint, source_label, signature, expires_at_ms)
    values (p_id, caller, p_source, host.owner_id, p_target, origin.public_key,
      origin.tls_fingerprint, origin.device_name, p_signature, p_expiry);
  return p_id;
end $$;
create function public.request_vmax_connect(p_id uuid, p_source uuid, p_target uuid,
  p_expiry bigint, p_signature text, p_token text default null)
returns uuid language sql security invoker set search_path = '' as $$
  select vmax_private.request_connect(p_id, p_source, p_target, p_expiry, p_signature, p_token)
$$;
revoke all on function vmax_private.find_profile(text,text), public.find_vmax_profile(text,text),
  vmax_private.request_connect(uuid,uuid,uuid,bigint,text,text),
  public.request_vmax_connect(uuid,uuid,uuid,bigint,text,text) from public, anon;
grant execute on function vmax_private.find_profile(text,text), public.find_vmax_profile(text,text),
  vmax_private.request_connect(uuid,uuid,uuid,bigint,text,text),
  public.request_vmax_connect(uuid,uuid,uuid,bigint,text,text) to authenticated;

alter publication supabase_realtime add table public.devices, public.connect_requests;
