#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <pthread.h>
#include <unistd.h>
#include <dlfcn.h>

// inklingfb — clean page-clear for the Magic Notebook, using xochitl's own scene.
//
// Hook-free and stable (no QMetaObject::activate hooks — those crash xovi's arm32
// trampoline). On trigger it walks the live QtQuick VISUAL tree to the active page's
// SceneController (DocumentView.sceneController), invokes clearLines +
// clearRootDocument (Qt::QueuedConnection, thread-safe from this worker thread), then
// update() on the scene views to refresh the panel. xochitl performs the erase and
// e-ink refresh itself — this is why it sidesteps the framebuffer-swap problem.
//
//   Trigger:  touch /tmp/inklingfb_clear     (deleted after handling)
//   Loads via xovi auto-load — plain LD_PRELOAD, no hooks, no kick shim.
//   All Qt entry points are resolved by dlsym against libQt6Core/Gui/Quick.
//
// NOTE: a QImage-ctor override to capture the panel buffer was tried here and REVERTED
// — hooking that hot, multi-threaded ctor serializes/rewrites it on xochitl's render
// path (xovi takes a per-hook mutex + un-hooks/re-hooks per original-call) and reliably
// trips xochitl's "Something went wrong" render error. Screen capture stays in the
// daemon via /proc/pid/mem (device/capture.rs). Keep this extension hook-free.

typedef struct { const char *name; const void *data; } GA;           // QGenericArgument
typedef int         (*invoke_fn)(void*, const char*, int, GA,GA,GA,GA,GA,GA,GA,GA,GA,GA,GA);
typedef const char *(*classname_fn)(const void*);
typedef void        (*findchildren_fn)(const void*, const void*, void*, int);
typedef void        (*allwindows_fn)(void*);                          // sret QWindowList
typedef void        (*childitems_fn)(void*, const void*);            // sret QList<QQuickItem*>
typedef void        (*qproperty_fn)(void*, const void*, const char*); // sret QVariant

static invoke_fn       p_invoke;
static classname_fn    p_className;
static findchildren_fn p_findchildren;
static allwindows_fn   p_allwindows;
static childitems_fn   p_childitems;
static qproperty_fn    p_qproperty;
static void           *g_qobj_smo;    // &QObject::staticMetaObject

// --- tiny Qt introspection helpers (all read-only, safe on any QObject) ---
static void* meta_of(void *o){ return ((void*(*)(void*))(*(void***)o)[0])(o); }   // metaObject() @ vtable[0]
static const char* cls(void *o){ if(!o) return 0; void *mo=meta_of(o); return mo?p_className(mo):0; }
static void* read_obj_prop(void *o, const char *name){
    char v[64]; for(int i=0;i<64;i++) v[i]=0; p_qproperty(v,o,name); return *(void**)v; // ptr stored inline in QVariant
}
// QMetaObject::superClass() is inline; superdata is the first field of QMetaObject.
static int is_quickitem(void *o){
    void *mo=meta_of(o);
    for(int i=0; mo && i<40; i++){ const char*c=p_className(mo); if(c&&!strcmp(c,"QQuickItem")) return 1; mo=*(void**)mo; }
    return 0;
}

// --- collect the active page's SceneController(s) + scene views ---
static void *g_seen[9000]; static int g_nseen;
static void *g_scs[16];    static int g_nscs;
static void *g_views[16];  static int g_nviews;
static int seen(void *p){ for(int i=0;i<g_nseen;i++) if(g_seen[i]==p) return 1; if(g_nseen<9000) g_seen[g_nseen++]=p; return 0; }

static void add_sc(void *sc){
    if(!sc){ return; } const char *c=cls(sc); if(!c||strcmp(c,"SceneController")) return;
    for(int i=0;i<g_nscs;i++) if(g_scs[i]==sc) return;
    if(g_nscs<16) g_scs[g_nscs++]=sc;
}
static void add_view(void *v){ for(int i=0;i<g_nviews;i++) if(g_views[i]==v) return; if(g_nviews<16) g_views[g_nviews++]=v; }

// Visual-only walk (QQuickItem::childItems) — the stable traversal. Reaches the
// DocumentView / DeviceSceneView that are actually rendered (i.e. the active page).
static void walk(void *item, int depth){
    if(!item || depth>70 || g_nseen>8000 || seen(item)) return;
    const char *c = cls(item); if(!c) return;
    if(!strcmp(c,"SceneController")){ add_sc(item); return; }
    if(strstr(c,"DocumentView") && !strstr(c,"Shortcuts")){ add_sc(read_obj_prop(item,"sceneController")); add_view(item); }
    else if(strstr(c,"DeviceScene")){ add_sc(read_obj_prop(item,"controller")); add_view(item); }
    if(is_quickitem(item)){
        void *cl[3]={0,0,0}; p_childitems(&cl,item);
        void **kids=(void**)cl[1]; long kn=(long)cl[2];
        for(long i=0;i<kn;i++) walk(kids[i], depth+1);
    }
}

static void locate(void){
    g_nseen=0; g_nscs=0; g_nviews=0;
    void *wl[3]={0,0,0}; p_allwindows(&wl);
    void **wins=(void**)wl[1]; long wn=(long)wl[2];
    for(long w=0; w<wn; w++){
        void *lst[3]={0,0,0}; p_findchildren(wins[w], g_qobj_smo, lst, 1);   // find QQuickRootItem
        void **arr=(void**)lst[1]; long n=(long)lst[2];
        for(long j=0;j<n;j++){ const char *c=cls(arr[j]); if(c && !strcmp(c,"QQuickRootItem")) walk(arr[j],0); }
    }
}

static void clear_page(void){
    locate();
    GA z = {0,0};
    for(int i=0;i<g_nscs;i++){
        p_invoke(g_scs[i], "clearLines",        2, z,z,z,z,z,z,z,z,z,z,z);   // ink
        p_invoke(g_scs[i], "clearRootDocument", 2, z,z,z,z,z,z,z,z,z,z,z);   // text
    }
    for(int i=0;i<g_nviews;i++)
        p_invoke(g_views[i], "update", 2, z,z,z,z,z,z,z,z,z,z,z);            // refresh the panel
    fprintf(stderr, "[inklingfb] cleared page (%d controller(s), %d view(s))\n", g_nscs, g_nviews);
}

static void* watcher(void* _){
    (void)_;
    for(;;){
        if(access("/tmp/inklingfb_clear", F_OK)==0){ clear_page(); unlink("/tmp/inklingfb_clear"); }
        usleep(120000);
    }
    return 0;
}

void _xovi_construct(void){
    p_invoke      = (invoke_fn) dlsym(RTLD_DEFAULT,"_ZN11QMetaObject12invokeMethodEP7QObjectPKcN2Qt14ConnectionTypeE22QGenericReturnArgument16QGenericArgumentS7_S7_S7_S7_S7_S7_S7_S7_S7_");
    p_className   = (classname_fn) dlsym(RTLD_DEFAULT,"_ZNK11QMetaObject9classNameEv");
    p_findchildren= (findchildren_fn) dlsym(RTLD_DEFAULT,"_Z23qt_qFindChildren_helperPK7QObjectRK11QMetaObjectP5QListIPvE6QFlagsIN2Qt15FindChildOptionEE");
    p_allwindows  = (allwindows_fn) dlsym(RTLD_DEFAULT,"_ZN15QGuiApplication10allWindowsEv");
    p_childitems  = (childitems_fn) dlsym(RTLD_DEFAULT,"_ZNK10QQuickItem10childItemsEv");
    p_qproperty   = (qproperty_fn) dlsym(RTLD_DEFAULT,"_ZNK7QObject8propertyEPKc");
    g_qobj_smo    = dlsym(RTLD_DEFAULT,"_ZN7QObject16staticMetaObjectE");
    fprintf(stderr, "[inklingfb] loaded (page-clear via SceneController)\n");
    pthread_t t; pthread_create(&t, NULL, watcher, NULL);
}
