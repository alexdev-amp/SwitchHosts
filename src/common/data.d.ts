import { ITreeNodeData } from './tree'

export type HostsType = 'local' | 'remote' | 'group' | 'folder'
export type FolderModeType = 0 | 1 | 2 // 0: 默认; 1: 单选; 2: 多选

export interface IDomainResolution {
  domain: string
  ips: string[]
  status: 'resolved' | 'stale' | 'failed'
  last_success?: string
  last_success_ms?: number
  error?: string
}

export interface IHostsListObject {
  id: string
  title?: string
  on?: boolean
  type?: HostsType

  // remote
  source?: 'url' | 'domain' // missing ⇒ 'url'; old domain entries use `url`
  url?: string
  domains?: string[]
  domain_results?: IDomainResolution[]
  domain_refresh_status?: 'complete' | 'partial' | 'failed'
  last_attempt?: string
  last_attempt_ms?: number
  last_refresh?: string
  last_refresh_ms?: number
  refresh_interval?: number // 单位：秒

  // hosts API source — synced machine entries under the single-choice
  // folder created by sync_hosts_api (see src-tauri/src/hosts_api_sync.rs).
  // These are plain `local` entries, not a distinct HostsType; these
  // fields just carry the extra metadata the sync wrote.
  tags?: string[]
  hosts_api_label?: string
  hosts_api_state?: string
  hosts_api_ip?: string | null

  // group
  include?: string[]

  // folder
  folder_mode?: FolderModeType
  folder_open?: boolean
  children?: IHostsListObject[]

  is_sys?: boolean

  [key: string]: any
}

export interface IHostsContentObject {
  id: string
  content: string

  [key: string]: any
}

export interface ITrashcanObject {
  data: IHostsListObject
  add_time_ms: number
  parent_id: string | null
}

export interface ITrashcanListObject extends ITrashcanObject, ITreeNodeData {
  id: string
  children?: ITrashcanListObject[]
  is_root?: boolean
  type?: HostsType | 'trashcan'

  [key: string]: any
}

export interface IHostsHistoryObject {
  id: string
  content: string
  add_time_ms: number
  label?: string
}

export type VersionType = string

export type IApplicationRecovery =
  { status: 'applied'; list: IHostsListObject[] } | { status: 'unknown' }

export interface IHostsBasicData {
  application_recovery?: IApplicationRecovery | null
  list: IHostsListObject[]
  trashcan: ITrashcanObject[]
  version: VersionType
}

export interface IOperationResult {
  success: boolean
  message?: string
  data?: any
  code?: string | number
}

export interface ICommandRunResult {
  _id?: string
  success: boolean
  stdout: string
  stderr: string
  add_time_ms: number
}
