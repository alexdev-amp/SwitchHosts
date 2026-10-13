/**
 * constants
 * @author: oldj
 * @homepage: https://oldj.net
 */

export const serverUrl = 'https://switchhosts.vercel.app'
export const homepageUrl = 'https://switchhosts.app'
export const downloadUrl = `${serverUrl}/download/`
export const sourceUrl = 'https://github.com/oldj/SwitchHosts'
export const feedbackUrl = 'https://github.com/oldj/SwitchHosts/issues'
export const defaultHttpApiPort = 50761

// Must match FOLDER_ID in src-tauri/src/hosts_api_sync.rs -- the fixed
// id of the single-choice folder the hosts-API source syncs machines
// into. Used to redirect "Edit" on that node to the connect dialog
// (endpoint/username/password) instead of the generic folder editor.
export const hostsApiFolderId = 'hosts-api-folder'
