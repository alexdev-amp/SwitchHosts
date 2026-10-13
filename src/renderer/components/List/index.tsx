/**
 * @author: oldj
 * @homepage: https://oldj.net
 */

import { IHostsListObject } from '@common/data'
import events from '@common/events'
import { findItemById, flatten, getNextSelectedItem, setOnStateOfItem } from '@common/hostsFn'
import { IFindShowSourceParam } from '@common/types'
import { Box, Button, Group, TextInput } from '@mantine/core'
import ApplicationRecoveryNotice from '@renderer/components/ApplicationRecoveryNotice'
import ItemIcon from '@renderer/components/ItemIcon'
import { Tree } from '@renderer/components/Tree'
import { actions, agent } from '@renderer/core/agent'
import { getErrorMessage, showErrorNotification } from '@renderer/core/notify'
import useOnBroadcast from '@renderer/core/useOnBroadcast'
import useConfigs from '@renderer/models/useConfigs'
import useHostsData from '@renderer/models/useHostsData'
import useI18n from '@renderer/models/useI18n'
import clsx from 'clsx'
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { BiChevronRight } from 'react-icons/bi'
import styles from './index.module.scss'
import ListItem from './ListItem'

interface Props {
  isTray?: boolean
}

const List = (props: Props) => {
  const { isTray } = props
  const {
    hostsData,
    loadHostsData,
    setList,
    applyList,
    applicationRecovery,
    currentHosts,
    setCurrentHosts,
  } = useHostsData()
  const recoveryPending = !!applicationRecovery
  const nodeAttr = useCallback(
    (item: IHostsListObject) => ({
      can_drag: !item.is_sys && !isTray && !recoveryPending,
      can_drop_before: !item.is_sys,
      can_drop_in: item.type === 'folder',
      can_drop_after: !item.is_sys,
    }),
    [isTray, recoveryPending],
  )
  const { configs } = useConfigs()
  const { lang } = useI18n()
  const [selectedIds, setSelectedIds] = useState<string[]>(isTray ? [] : [currentHosts?.id || '0'])
  const [showList, setShowList] = useState<IHostsListObject[]>([])
  const [search, setSearch] = useState('')
  const [selectedTags, setSelectedTags] = useState<Set<string>>(new Set())

  // Hundreds of hosts is the normal case for an API-backed source, so
  // search/tag filtering only hides rows for display -- it never touches
  // `showList`/`hostsData.list` themselves, which also serve as the
  // write-back source for drag-and-drop reordering (see the Tree
  // `onChange` below). Filtering `data` directly would silently drop
  // every hidden node the next time the user reorders anything.
  const allTags = useMemo(() => {
    const set = new Set<string>()
    flatten(hostsData.list).forEach((item) => (item.tags || []).forEach((t) => set.add(t)))
    return Array.from(set).sort()
  }, [hostsData.list])

  const filterActive = search.trim() !== '' || selectedTags.size > 0

  const nodeMatchesFilter = useCallback(
    (item: IHostsListObject) => {
      if (selectedTags.size > 0 && !(item.tags || []).some((t) => selectedTags.has(t))) {
        return false
      }
      const needle = search.trim().toLowerCase()
      if (!needle) return true
      return (
        (item.title || '').toLowerCase().includes(needle) ||
        (item.tags || []).some((t) => t.toLowerCase().includes(needle))
      )
    },
    [search, selectedTags],
  )

  // A folder stays visible whenever any descendant matches, even if the
  // folder itself doesn't -- otherwise filtering would hide the very
  // thing a match is nested inside.
  const visibleIds = useMemo(() => {
    if (!filterActive) return null
    const ids = new Set<string>()
    const walk = (items: IHostsListObject[]): boolean => {
      let anyVisible = false
      items.forEach((item) => {
        const childVisible = item.children ? walk(item.children) : false
        if (nodeMatchesFilter(item) || childVisible) {
          ids.add(item.id)
          anyVisible = true
        }
      })
      return anyVisible
    }
    walk(hostsData.list)
    return ids
  }, [hostsData.list, filterActive, nodeMatchesFilter])
  const latestHostsData = useRef(hostsData)
  const changeRevision = useRef(0)
  const remoteContentApplyRef = useRef({
    isApplying: false,
    pendingIds: new Set<string>(),
  })

  useEffect(() => {
    latestHostsData.current = hostsData
    /* eslint-disable react-hooks/set-state-in-effect -- showList also mutated by drag onChange; keep as state synced from hostsData */
    if (!isTray) {
      setShowList([
        {
          id: '0',
          title: lang.system_hosts,
          is_sys: true,
        },
        ...hostsData.list,
      ])
    } else {
      setShowList([...hostsData.list])
    }
    /* eslint-enable react-hooks/set-state-in-effect */
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostsData])

  useEffect(() => {
    if (isTray || !currentHosts) return
    if (!hostsData.trashcan.find((item) => item.data.id === currentHosts.id)) return

    // eslint-disable-next-line react-hooks/set-state-in-effect -- clear selection when current item moves to trashcan
    setSelectedIds([])
  }, [currentHosts, hostsData.trashcan, isTray])

  const onToggleItem = async (id: string, on: boolean) => {
    if (!configs?.write_mode) {
      agent.broadcast(events.show_set_write_mode, { id, on })
      return
    }

    const success = await applyList((list) =>
      setOnStateOfItem(
        list,
        id,
        on,
        configs?.choice_mode ?? 0,
        configs?.multi_chose_folder_switch_all ?? false,
      ),
    )
    if (success === true) {
      agent.broadcast(events.set_hosts_on_status, id, on)
    } else if (success === false) {
      agent.broadcast(events.set_hosts_on_status, id, !on)
    }
  }

  const applyChangedRemoteHostsToSystem = async (ids: string[]) => {
    if (isTray) return

    const applyState = remoteContentApplyRef.current
    for (const id of ids) {
      if (id) applyState.pendingIds.add(id)
    }
    if (applyState.pendingIds.size === 0 || applyState.isApplying) {
      return
    }

    applyState.isApplying = true
    try {
      while (applyState.pendingIds.size > 0) {
        const changedIds = Array.from(applyState.pendingIds)
        applyState.pendingIds.clear()

        const list: IHostsListObject[] = await actions.getList()
        const hasEnabledChangedHosts = changedIds.some((id) => {
          const hosts = findItemById(list, id)
          return !!hosts?.on
        })
        if (!hasEnabledChangedHosts) continue

        await applyList()
      }
    } catch (e) {
      console.error(e)
    } finally {
      applyState.isApplying = false
    }

    // Race guard: an event arriving after `while` exits but before we
    // clear `isApplying` would have early-returned (saw isApplying=true)
    // and left ids stranded in pendingIds. Re-trigger if so.
    if (applyState.pendingIds.size > 0) {
      applyChangedRemoteHostsToSystem([]).catch((e) => console.error(e))
    }
  }

  useOnBroadcast(
    events.toggle_item,
    (id: string, on: boolean) => {
      if (isTray) return
      return onToggleItem(id, on).catch((error: unknown) => {
        showErrorNotification({ title: lang.fail, message: getErrorMessage(error, lang.fail) })
      })
    },
    [hostsData, configs, isTray],
  )
  useOnBroadcast(
    events.tray_list_updated,
    () => {
      if (!isTray) return
      loadHostsData().catch((error: unknown) => console.error(error))
    },
    [isTray],
  )

  useOnBroadcast(
    events.move_to_trashcan,
    async (ids: string[]) => {
      try {
        await actions.moveManyToTrashcan(ids)
        await loadHostsData()

        if (currentHosts && ids.includes(currentHosts.id)) {
          // 选中删除指定节点后的兄弟节点
          const nextItem = getNextSelectedItem(hostsData.list, (i) => ids.includes(i.id))
          setCurrentHosts(nextItem || null)
          setSelectedIds(nextItem ? [nextItem.id] : [])
        }
      } catch (error) {
        showErrorNotification({ title: lang.fail, message: getErrorMessage(error, lang.fail) })
      }
    },
    [currentHosts, hostsData],
  )

  useOnBroadcast(
    events.select_hosts,
    async (id: string, waitMs: number = 0) => {
      if (isTray) return
      const hosts = findItemById(hostsData.list, id)
      if (!hosts) {
        if (waitMs > 0) {
          setTimeout(() => {
            agent.broadcast(events.select_hosts, id, waitMs - 50)
          }, 50)
        }
        return
      }

      setCurrentHosts(hosts)
      setSelectedIds([id])
    },
    [hostsData, isTray],
  )

  useOnBroadcast(events.reload_list, loadHostsData)
  // Refreshes also change per-domain status when the generated content is
  // unchanged. Reload the list so reselecting an item keeps those results.
  useOnBroadcast(events.hosts_refreshed, () => {
    loadHostsData().catch((error: unknown) => console.error(error))
  })
  useOnBroadcast(
    events.hosts_content_changed,
    (hostsId: string) => {
      applyChangedRemoteHostsToSystem([hostsId]).catch((e) => console.error(e))
    },
    [currentHosts, hostsData, isTray, lang],
  )

  useOnBroadcast(
    events.hosts_content_changed_batch,
    (hostsIds: string[]) => {
      applyChangedRemoteHostsToSystem(Array.isArray(hostsIds) ? hostsIds : []).catch((e) =>
        console.error(e),
      )
    },
    [currentHosts, hostsData, isTray, lang],
  )

  useOnBroadcast(events.show_source, async (params: IFindShowSourceParam) => {
    agent.broadcast(events.select_hosts, params.item_id)
  })

  return (
    <div className={styles.root}>
      <ApplicationRecoveryNotice />
      {!isTray && (hostsData.list.length > 0 || allTags.length > 0) && (
        <Box className={styles.filter_bar}>
          <TextInput
            aria-label={lang.search}
            placeholder={lang.search}
            size="xs"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
          {allTags.length > 0 && (
            <Group gap="4px" mt="6px" wrap="wrap">
              {allTags.map((tag) => {
                const active = selectedTags.has(tag)
                return (
                  <Button
                    key={tag}
                    size="compact-xs"
                    variant={active ? 'filled' : 'outline'}
                    onClick={() =>
                      setSelectedTags((prev) => {
                        const next = new Set(prev)
                        if (active) next.delete(tag)
                        else next.add(tag)
                        return next
                      })
                    }
                  >
                    {tag}
                  </Button>
                )
              })}
              {selectedTags.size > 0 && (
                <Button size="compact-xs" variant="subtle" onClick={() => setSelectedTags(new Set())}>
                  {lang.clear_filters}
                </Button>
              )}
            </Group>
          )}
        </Box>
      )}
      <Tree
        data={showList}
        selectedIds={selectedIds}
        onChange={(list) => {
          const revision = ++changeRevision.current
          setShowList(list)
          const restoreView = () => {
            if (revision !== changeRevision.current) return
            const saved = latestHostsData.current.list
            setShowList(
              isTray ? [...saved] : [{ id: '0', title: lang.system_hosts, is_sys: true }, ...saved],
            )
          }
          const newUserList = list.filter((i) => !i.is_sys)

          const enabledIdSeq = (l: IHostsListObject[]) =>
            flatten(l)
              .filter((i) => i.on)
              .map((i) => i.id)
              .join('\n')

          if (enabledIdSeq(hostsData.list) !== enabledIdSeq(newUserList) && configs?.write_mode) {
            applyList(newUserList)
              .then((success) => {
                if (success === false) restoreView()
              })
              .catch((error: unknown) => {
                restoreView()
                showErrorNotification({
                  title: lang.fail,
                  message: getErrorMessage(error, lang.fail),
                })
              })
          } else {
            setList(newUserList).catch((error: unknown) => {
              restoreView()
              console.error(error)
            })
          }
        }}
        onSelect={(ids: string[]) => {
          if (isTray) return
          setSelectedIds(ids)
        }}
        nodeRender={(data) => {
          if (visibleIds && !data.is_sys && !visibleIds.has(data.id)) return null
          return <ListItem key={data.id} data={data} isTray={isTray} selectedIds={selectedIds} />
        }}
        collapseArrow={
          <div
            style={{
              width: '20px',
              height: '20px',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
            }}
          >
            <BiChevronRight />
          </div>
        }
        nodeAttr={nodeAttr}
        draggingNodeRender={(data) => {
          return (
            <div className={clsx(styles.for_drag)}>
              <span className={clsx(styles.icon, data.type === 'folder' && styles.folder)}>
                <ItemIcon
                  type={data.is_sys ? 'system' : data.type}
                  isCollapsed={data.is_collapsed}
                />
              </span>
              <span>
                {data.title || lang.untitled}
                {selectedIds.length > 1 ? (
                  <span className={styles.items_count}>
                    {selectedIds.length} {lang.items}
                  </span>
                ) : null}
              </span>
            </div>
          )
        }}
        nodeClassName={styles.node}
        nodeDropInClassName={styles.node_drop_in}
        nodeSelectedClassName={styles.node_selected}
        nodeCollapseArrowClassName={styles.arrow}
        allowedMultipleSelection={true}
      />
    </div>
  )
}

export default List
