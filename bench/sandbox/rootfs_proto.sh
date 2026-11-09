set -e
B=$HOME/rb; rm -rf $B; mkdir -p $B/small $B/big
cp -a $HOME/zt/../fx_busy/. $B/small 2>/dev/null || { mkdir -p $B/small/bin; cp /usr/bin/busybox $B/small/bin/; }
cp -a /usr/lib/x86_64-linux-gnu /usr/share $B/big/ 2>/dev/null
echo "small base: $(du -sh $B/small | cut -f1), $(find $B/small | wc -l) files; big base: $(du -sh $B/big | cut -f1), $(find $B/big | wc -l) files"
ns() { date +%s%N; }
for base in small big; do
  echo "== base: $base"
  for kind in overlay fuse copy; do
    t=()
    for i in 1 2 3 4 5; do
      rm -rf $B/w; mkdir -p $B/w/{u,w,m,c}
      case $kind in
        overlay) s=$(ns); unshare -Urm sh -c "mount -t overlay overlay -o lowerdir=$B/$base,upperdir=$B/w/u,workdir=$B/w/w $B/w/m && touch $B/w/m/newfile && ls $B/w/m >/dev/null"; e=$(ns);;
        fuse) s=$(ns); unshare -Urm sh -c "fuse-overlayfs -o lowerdir=$B/$base,upperdir=$B/w/u,workdir=$B/w/w $B/w/m && touch $B/w/m/newfile && ls $B/w/m >/dev/null" 2>/dev/null; e=$(ns);;
        copy) s=$(ns); cp -a --reflink=auto $B/$base/. $B/w/c; touch $B/w/c/newfile; e=$(ns);;
      esac
      t+=($(( (e-s)/1000000 )))
      chmod -R u+rwx $B/w 2>/dev/null; rm -rf $B/w
    done
    echo "$kind: ${t[*]} ms"
  done
done
